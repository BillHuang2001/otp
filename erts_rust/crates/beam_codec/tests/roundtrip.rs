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

//! Exhaustive encode → decode round-trips plus negative cases.
//!
//! The corpus is property-style over boundary values and is driven entirely through
//! the **safe** public API — `beam_alloc::Heap`, the `beam_term` immediates/atoms and
//! the `beam_codec::term` block accessors. The library is `#![deny(unsafe_code)]` and
//! these tests keep that promise: no `unsafe` here.
//!
//! For every constructed term `T` the core property is
//! `encode_term(T) =:= encode_term(decode_term(encode_term(T)))` — the wire image is
//! byte-stable — *and* the decoded term is checked **structurally** against `T`
//! (tuple arity/elements, cons spines, `big_parts`/`float_value`/`flatmap_parts`/
//! `external_*_parts`, and `signed_value`/atom names for immediates). Floats are
//! compared by bit pattern (`to_bits`), never with `==`.
//!
//! Coverage: boundary integers (small/`INTEGER_EXT`/bignum/sign), bignums across the
//! wire byte-count boundaries including high-zero stripping, every atom tag (short,
//! long, multi-byte UTF-8, plus Latin-1), floats including `±0`, `±inf`, `NaN` (quiet
//! and signaling) and subnormals, tuples at the arity boundaries, proper/improper lists
//! and byte lists up to the `STRING_EXT`/`LIST_EXT` switchover, small maps,
//! pids/ports/references and both fun tags — `EXPORT_EXT` (`113`) and `NEW_FUN_EXT`
//! (`112`), standalone and nested inside tuples/lists/maps.
//!
//! Two documented *non*-byte-identical transformations are asserted explicitly: a
//! bignum whose magnitude fits the 60-bit small range decodes to a small (matching the
//! C `dec_term`), and a legacy `FLOAT_EXT` re-encodes as `NEW_FLOAT_EXT`.
//!
//! Negative cases cover truncation at every byte offset (including every `NEW_FUN_EXT`
//! field boundary), unknown/unsupported/obsolete tags (the removed `FUN_EXT`), the fun
//! field rules (non-small `old_index`/`old_uniq`, a non-pid creator, `num_free` past
//! its 8-bit field, missing environment terms), over-long atoms and bignums
//! (`BeamError::SystemLimit`), oversized maps (`error::unsupported`), a malformed
//! bignum sign and trailing bytes. Encoding a local (`make_fun3`) fun — which has no
//! faithful wire image — raises `unsupported_local_fun`, never a mis-encoded term.
//! Every malformed input must yield an error — never a panic, never a silently
//! truncated or mis-encoded term.

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::{
    atom_val, is_atom, is_list, is_nil, is_small, make_atom, make_boxed, make_fun_header,
    make_small, signed_value, AtomTable, Eterm, MAX_SMALL, MIN_SMALL, NIL,
};

use beam_codec::compress::{compress, decompress};
use beam_codec::error::{is_unsupported, is_unsupported_local_fun};
use beam_codec::term::{
    big_parts, bitstring_bytes, cons_head, cons_tail, external_pid_parts, external_port_parts, external_ref_parts,
    flatmap_parts, float_value, fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index,
    fun_kind_term, fun_module, fun_num_free_term, fun_old_index, fun_old_uniq, fun_uniq_bytes,
    make_big, make_binary, make_external_pid, make_external_port, make_external_ref, make_flatmap,
    make_float, make_fun_external, make_fun_imported, tuple_arity, tuple_element,
};
use beam_codec::{
    decode_term, encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    AtomEncoding, Decoder, Encoder, ATOM_EXT, ATOM_UTF8_EXT, BINARY_EXT, COMPRESSED_EXT, EXPORT_EXT,
    FLOAT_EXT, FunKind, FunPid, INTEGER_EXT, LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, MAP_EXT,
    NEWER_REFERENCE_EXT, NEW_FLOAT_EXT, NEW_FUN_EXT, NEW_PID_EXT, NEW_PORT_EXT, NEW_REFERENCE_EXT,
    NIL_EXT, SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT,
    V4_PORT_EXT, VERSION_MAGIC,
};

// ---------------------------------------------------------------------------
// Safe construction / inspection helpers
// ---------------------------------------------------------------------------

/// A decode heap of `words` term words.
fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// Builds a cons list from `elems` (in order) terminated by `tail`.
fn list(h: &mut Heap, elems: &[Eterm], tail: Eterm) -> Eterm {
    let mut acc = tail;
    for &e in elems.iter().rev() {
        acc = h.cons(e, acc).expect("heap sized for the list");
    }
    acc
}

/// Builds the list of smalls `bytes[0], bytes[1], …`.
fn byte_list(h: &mut Heap, bytes: &[u8]) -> Eterm {
    let mut acc = NIL;
    for &b in bytes.iter().rev() {
        acc = h
            .cons(make_small(i64::from(b)), acc)
            .expect("heap sized for the list");
    }
    acc
}

/// Walks a list spine, returning `(heads, tail)`. Iterative: never recurses, so it is
/// safe for lists tens of thousands of cells long.
fn collect_list(term: Eterm) -> (Vec<Eterm>, Eterm) {
    let mut heads = Vec::new();
    let mut cursor = term;
    while is_list(cursor) {
        heads.push(cons_head(cursor).expect("is_list ⇒ a cons cell"));
        cursor = cons_tail(cursor).expect("is_list ⇒ a cons cell");
    }
    (heads, cursor)
}

/// Drops the high-order (most significant) zero digits of a little-endian
/// `ErtsDigit` magnitude.
fn strip_high_zero_digits(digits: &[u64]) -> &[u64] {
    let mut end = digits.len();
    while end > 0 && digits[end - 1] == 0 {
        end -= 1;
    }
    &digits[..end]
}

/// The non-negative value of a little-endian `ErtsDigit` magnitude that fits an
/// `i64`.
fn digits_value(digits: &[u64]) -> i64 {
    let mut v: i128 = 0;
    for &digit in digits.iter().rev() {
        v = (v << 64) | i128::from(digit);
    }
    v as i64
}

/// Structural equality of two terms that live in (possibly different) heaps and atom
/// tables. Atoms are the only identity that is *not* stable across tables, so they are
/// compared by **name**; floats are compared by bit pattern. List tails are compared
/// iteratively to keep deep spines off the stack.
fn assert_term_eq(a: Eterm, ta: &AtomTable, b: Eterm, tb: &AtomTable) {
    if is_small(a) {
        assert!(is_small(b), "expected a small integer, got {b:?}");
        assert_eq!(signed_value(a), signed_value(b));
        return;
    }
    if is_nil(a) {
        assert!(is_nil(b), "expected [], got {b:?}");
        return;
    }
    if is_atom(a) {
        assert!(is_atom(b), "expected an atom, got {b:?}");
        assert_eq!(ta.name(atom_val(a)), tb.name(atom_val(b)));
        return;
    }
    if is_list(a) {
        assert!(is_list(b), "expected a list, got {b:?}");
        let (mut x, mut y) = (a, b);
        loop {
            assert_term_eq(
                cons_head(x).expect("is_list"),
                ta,
                cons_head(y).expect("list length mismatch"),
                tb,
            );
            let (xt, yt) = (
                cons_tail(x).expect("is_list"),
                cons_tail(y).expect("is_list"),
            );
            if is_list(xt) {
                assert!(is_list(yt), "list length mismatch");
                x = xt;
                y = yt;
                continue;
            }
            // The (improper or NIL) tail terminates the spine.
            assert_term_eq(xt, ta, yt, tb);
            return;
        }
    }
    // Funs (`EXPORT_EXT`/`NEW_FUN_EXT`): the two sides may live in different heaps and
    // atom tables, so module/function atoms (and the creator pid's node) are compared
    // by **name** and the environment recursively. The classification is exact — a fun
    // header carries a distinct subtag — so this cannot shadow another family.
    if let Ok(kind_a) = fun_kind_term(a) {
        let kind_b = fun_kind_term(b).expect("expected a fun");
        assert_eq!(kind_a, kind_b, "fun kind mismatch");
        assert_eq!(
            fun_arity_term(a).expect("a fun has an arity"),
            fun_arity_term(b).expect("a fun has an arity"),
            "fun arity mismatch"
        );
        assert_eq!(
            fun_num_free_term(a).expect("a fun has a num_free"),
            fun_num_free_term(b).expect("a fun has a num_free"),
            "fun env size mismatch"
        );
        match kind_a {
            FunKind::External => {
                assert_eq!(
                    ta.name(fun_module(a).expect("external fun module")),
                    tb.name(fun_module(b).expect("external fun module")),
                    "fun module mismatch"
                );
                assert_eq!(
                    ta.name(fun_function(a).expect("external fun function")),
                    tb.name(fun_function(b).expect("external fun function")),
                    "fun function mismatch"
                );
            }
            FunKind::Imported => {
                assert_eq!(
                    ta.name(fun_module(a).expect("imported fun module")),
                    tb.name(fun_module(b).expect("imported fun module")),
                    "fun module mismatch"
                );
                assert_eq!(
                    fun_uniq_bytes(a).expect("imported fun uniq"),
                    fun_uniq_bytes(b).expect("imported fun uniq"),
                    "fun uniq mismatch"
                );
                assert_eq!(fun_index(a).unwrap(), fun_index(b).unwrap());
                assert_eq!(fun_old_index(a).unwrap(), fun_old_index(b).unwrap());
                assert_eq!(fun_old_uniq(a).unwrap(), fun_old_uniq(b).unwrap());
                let (pa, pb) = (
                    fun_creator_pid(a).expect("imported fun creator"),
                    fun_creator_pid(b).expect("imported fun creator"),
                );
                assert_eq!(ta.name(pa.node), tb.name(pb.node), "creator node mismatch");
                assert_eq!(
                    (pa.number, pa.serial, pa.creation),
                    (pb.number, pb.serial, pb.creation),
                    "creator pid mismatch"
                );
                let (ea, eb) = (fun_env(a).unwrap(), fun_env(b).unwrap());
                assert_eq!(ea.len(), eb.len(), "fun env length mismatch");
                for i in 0..ea.len() {
                    assert_term_eq(ea[i], ta, eb[i], tb);
                }
            }
            FunKind::Local => {
                // A local fun inlines no module/uniq/creator pid.
                assert_eq!(fun_index(a).unwrap(), fun_index(b).unwrap());
                let (ea, eb) = (fun_env(a).unwrap(), fun_env(b).unwrap());
                assert_eq!(ea.len(), eb.len(), "local fun env length mismatch");
                for i in 0..ea.len() {
                    assert_term_eq(ea[i], ta, eb[i], tb);
                }
            }
        }
        return;
    }
    if let Ok((neg_a, dig_a)) = big_parts(a) {
        let (neg_b, dig_b) = big_parts(b).expect("expected a bignum");
        assert_eq!(neg_a, neg_b, "bignum sign mismatch");
        // High-order zero digits are not significant.
        assert_eq!(strip_high_zero_digits(dig_a), strip_high_zero_digits(dig_b));
        return;
    }
    if let Ok(fa) = float_value(a) {
        let fb = float_value(b).expect("expected a float");
        assert_eq!(fa.to_bits(), fb.to_bits(), "float bits differ");
        return;
    }
    if let Ok((bytes_a, off_a, size_a)) = bitstring_bytes(a) {
        let (bytes_b, off_b, size_b) = bitstring_bytes(b).expect("expected a bitstring");
        assert_eq!(size_a, size_b, "bitstring size mismatch");
        // Compare the significant bits (MSB-first inside each byte).
        for i in 0..size_a {
            let bit_a = (bytes_a[(off_a + i) / 8] >> (7 - ((off_a + i) % 8))) & 1;
            let bit_b = (bytes_b[(off_b + i) / 8] >> (7 - ((off_b + i) % 8))) & 1;
            assert_eq!(bit_a, bit_b, "bitstring bit {i} differs");
        }
        return;
    }
    if let Ok(arity) = tuple_arity(a) {
        assert_eq!(tuple_arity(b).expect("expected a tuple"), arity);
        for i in 0..arity {
            assert_term_eq(
                tuple_element(a, i).expect("in-range"),
                ta,
                tuple_element(b, i).expect("in-range"),
                tb,
            );
        }
        return;
    }
    if let Ok((keys_a, vals_a)) = flatmap_parts(a) {
        let (keys_b, vals_b) = flatmap_parts(b).expect("expected a flat map");
        assert_eq!(keys_a.len(), keys_b.len());
        assert_eq!(vals_a.len(), vals_b.len());
        for i in 0..keys_a.len() {
            assert_term_eq(keys_a[i], ta, keys_b[i], tb);
            assert_term_eq(vals_a[i], ta, vals_b[i], tb);
        }
        return;
    }
    if let Ok((node, number, serial, creation)) = external_pid_parts(a) {
        let (n2, num2, ser2, cre2) = external_pid_parts(b).expect("expected a pid");
        assert_term_eq(node, ta, n2, tb);
        assert_eq!((number, serial, creation), (num2, ser2, cre2));
        return;
    }
    if let Ok((node, number, creation)) = external_port_parts(a) {
        let (n2, num2, cre2) = external_port_parts(b).expect("expected a port");
        assert_term_eq(node, ta, n2, tb);
        assert_eq!((number, creation), (num2, cre2));
        return;
    }
    if let Ok((node, ids, creation)) = external_ref_parts(a) {
        let (n2, ids2, cre2) = external_ref_parts(b).expect("expected a reference");
        assert_term_eq(node, ta, n2, tb);
        assert_eq!(ids, ids2);
        assert_eq!(creation, cre2);
        return;
    }
    panic!("assert_term_eq does not handle the term {a:?}");
}

/// The core property: `T` → bytes `b1` → `T2` → bytes `b2`, with `b1 == b2` (byte
/// stability), `b1` a `term_to_binary` image (leading version byte) and `T2`
/// structurally equal to `T`.
///
/// Returns the image, the decoded term and the heap/atom-table that own it — the
/// caller must keep the heap alive for as long as it inspects the returned term.
fn roundtrip(term: Eterm, atoms: &AtomTable, cap: usize) -> (Vec<u8>, Eterm, Heap, AtomTable) {
    let b1 = encode_term(term, atoms).expect("encode");
    assert_eq!(
        b1.first(),
        Some(&VERSION_MAGIC),
        "term_to_binary must start with 131"
    );

    let mut heap2 = heap(cap);
    let mut atoms2 = AtomTable::new();
    let term2 = decode_term(&b1, &mut heap2, &mut atoms2).expect("decode");

    let b2 = encode_term(term2, &atoms2).expect("re-encode");
    assert_eq!(b1, b2, "encode/decode/encode must be byte-stable");
    assert_term_eq(term, atoms, term2, &atoms2);

    (b1, term2, heap2, atoms2)
}

// ---------------------------------------------------------------------------
// Integers
// ---------------------------------------------------------------------------

/// Encodes `v` as a small, decodes it, and checks the value and the byte-stability.
fn check_small(v: i64) {
    let atoms = AtomTable::new();
    let (_b, t2, _h, _a) = roundtrip(make_small(v), &atoms, 64);
    assert!(is_small(t2), "value {v} must decode to a small");
    assert_eq!(signed_value(t2), v);
}

#[test]
fn integers_round_trip_at_every_boundary() {
    for &v in &[
        0i64,
        1,
        255,
        256,
        -1,
        -256,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        MAX_SMALL,
        MIN_SMALL,
        MAX_SMALL - 1,
        MIN_SMALL + 1,
    ] {
        check_small(v);
    }
}

#[test]
fn integer_tag_policy_matches_the_c_widths() {
    let atoms = AtomTable::new();
    for &(v, tag) in &[
        (0i64, SMALL_INTEGER_EXT),
        (255, SMALL_INTEGER_EXT),
        (256, INTEGER_EXT),
        (-1, INTEGER_EXT),
        (i64::from(i32::MAX), INTEGER_EXT),
        (i64::from(i32::MIN), INTEGER_EXT),
        // Out of the signed 32-bit range: the magnitude becomes a bignum block.
        (i64::from(i32::MAX) + 1, SMALL_BIG_EXT),
        (i64::from(i32::MIN) - 1, SMALL_BIG_EXT),
        (MAX_SMALL, SMALL_BIG_EXT),
        (MIN_SMALL, SMALL_BIG_EXT),
    ] {
        let b = encode_term(make_small(v), &atoms).expect("encode");
        assert_eq!(b[1], tag, "value {v} must use tag {tag}");
    }

    // Byte-exact boundary encodings.
    assert_eq!(
        encode_term(make_small(0), &atoms).unwrap(),
        vec![131, 97, 0]
    );
    assert_eq!(
        encode_term(make_small(255), &atoms).unwrap(),
        vec![131, 97, 255]
    );
    assert_eq!(
        encode_term(make_small(256), &atoms).unwrap(),
        vec![131, 98, 0, 0, 1, 0]
    );
    assert_eq!(
        encode_term(make_small(-1), &atoms).unwrap(),
        vec![131, 98, 255, 255, 255, 255]
    );
    assert_eq!(
        encode_term(make_small(i64::from(i32::MAX) + 1), &atoms).unwrap(),
        vec![131, 110, 4, 0, 0, 0, 0, 128]
    );
    assert_eq!(
        encode_term(make_small(MAX_SMALL), &atoms).unwrap(),
        vec![131, 110, 8, 0, 255, 255, 255, 255, 255, 255, 255, 7]
    );
    assert_eq!(
        encode_term(make_small(MIN_SMALL), &atoms).unwrap(),
        vec![131, 110, 8, 1, 0, 0, 0, 0, 0, 0, 0, 8]
    );
}

// ---------------------------------------------------------------------------
// Bignums
// ---------------------------------------------------------------------------

/// Little-endian `ErtsDigit`s whose base-256 rendering is exactly `len` bytes, with a
/// non-zero most significant byte so nothing is stripped and, once `len >= 8`, the
/// magnitude exceeds `MAX_SMALL`.
fn big_digits(len: usize) -> Vec<u64> {
    assert!(len >= 1, "a zero-width bignum is illegal");
    /// Eight significant `0x37` bytes, so this digit never shrinks the byte count.
    const FULL_DIGIT: u64 = 0x3737_3737_3737_3737;
    let (full, rest) = (len / 8, len % 8);
    let mut digits = vec![FULL_DIGIT; full];
    if rest > 0 {
        // A digit whose most significant byte sits at index `rest - 1`.
        digits.push((1u64 << (8 * rest)) - 1);
    }
    digits
}

#[test]
fn bignums_round_trip_across_byte_counts() {
    // `SMALL_BIG_EXT` carries up to 255 base-256 bytes, `LARGE_BIG_EXT` up to
    // `BIG_ARITY_MAX * sizeof(ErtsDigit)` (65535 * 8 = 524280). `len >= 8` with a
    // non-zero most significant byte guarantees the magnitude is out of the 60-bit
    // small range, so the value round-trips as a bignum byte-for-byte.
    for &len in &[8usize, 9, 100, 255, 256, 65_535, 65_536] {
        for &negative in &[false, true] {
            let atoms = AtomTable::new();
            let cap = 1 + len / 8 + 64;
            let mut h1 = heap(cap);
            let digits = big_digits(len);
            let t = make_big(&mut h1, negative, &digits).expect("bignum fits");

            let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, cap);

            assert!(!is_small(t2), "a wide magnitude must stay a bignum");
            let (neg2, dig2) = big_parts(t2).expect("decoded bignum");
            assert_eq!(neg2, negative);
            assert_eq!(dig2, &digits[..]);

            if len <= 255 {
                assert_eq!(b1[1], SMALL_BIG_EXT);
                assert_eq!(usize::from(b1[2]), len);
                assert_eq!(b1[3], u8::from(negative));
            } else {
                assert_eq!(b1[1], LARGE_BIG_EXT);
                assert_eq!(
                    u32::from_be_bytes([b1[2], b1[3], b1[4], b1[5]]) as usize,
                    len
                );
                assert_eq!(b1[6], u8::from(negative));
            }
        }
    }
}

#[test]
fn bignum_high_zero_digits_are_stripped() {
    let atoms = AtomTable::new();
    // Nine significant bytes, with a zero most significant digit appended: the wire
    // carries only the significant ones (`big_bytes`, `big.c:3405`).
    let mut digits = big_digits(9);
    digits.push(0);
    assert_eq!(digits.len(), 3);

    let mut h1 = heap(64);
    let t = make_big(&mut h1, false, &digits).expect("bignum fits");
    let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, 64);

    assert_eq!(b1[1], SMALL_BIG_EXT);
    assert_eq!(
        usize::from(b1[2]),
        9,
        "the high zero digit is not on the wire"
    );
    assert!(!is_small(t2));
    let (neg, dig) = big_parts(t2).expect("decoded bignum");
    assert!(!neg);
    assert_eq!(dig, &big_digits(9)[..]);
}

/// A bignum whose magnitude fits the 60-bit small range decodes to a **small** (the C
/// `dec_term` behaviour), so the round-trip is value-preserving, not byte-identical.
fn assert_bignum_canonicalizes_to_small(digits: &[u64], negative: bool) {
    let atoms = AtomTable::new();
    let mut h1 = heap(64);
    let t = make_big(&mut h1, negative, digits).expect("bignum fits");

    let b1 = encode_term(t, &atoms).expect("encode");
    assert_eq!(b1[1], SMALL_BIG_EXT);

    let mut h2 = heap(64);
    let mut a2 = AtomTable::new();
    let t2 = decode_term(&b1, &mut h2, &mut a2).expect("decode");
    assert!(is_small(t2), "a small-fitting big must decode to a small");

    let magnitude = digits_value(strip_high_zero_digits(digits));
    let expected = if negative { -magnitude } else { magnitude };
    assert_eq!(signed_value(t2), expected);

    // The canonical re-encoding is the small-int form, and it is idempotent.
    let b2 = encode_term(t2, &a2).expect("re-encode");
    assert_eq!(b2, encode_term(make_small(expected), &atoms).unwrap());

    let mut h3 = heap(64);
    let mut a3 = AtomTable::new();
    let t3 = decode_term(&b2, &mut h3, &mut a3).expect("decode again");
    assert!(is_small(t3));
    assert_eq!(signed_value(t3), expected);
}

#[test]
fn small_fitting_bignums_canonicalize_to_smalls() {
    for &(len, negative) in &[
        (1usize, false),
        (1, true),
        (2, false),
        (2, true),
        (7, false),
        (7, true),
    ] {
        assert_bignum_canonicalizes_to_small(&big_digits(len), negative);
    }
    // High zero digits stripped down to a single significant digit.
    assert_bignum_canonicalizes_to_small(&[0xFF, 0], false);
}

// ---------------------------------------------------------------------------
// Atoms
// ---------------------------------------------------------------------------

#[test]
fn short_utf8_atoms_use_small_atom_utf8() {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("ok"));

    let (b1, t2, _h, a2) = roundtrip(a, &atoms, 64);
    assert_eq!(b1, vec![131, 119, 2, b'o', b'k']);
    assert!(is_atom(t2));
    assert_eq!(a2.name(atom_val(t2)).unwrap(), "ok");
}

#[test]
fn max_length_and_multibyte_atoms_round_trip() {
    // 255 ASCII characters: still `SMALL_ATOM_UTF8_EXT` (one-byte length).
    {
        let name = "a".repeat(255);
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern(&name));
        let (b1, t2, _h, a2) = roundtrip(a, &atoms, 64);
        assert_eq!(b1[1], SMALL_ATOM_UTF8_EXT);
        assert_eq!(usize::from(b1[2]), 255);
        assert_eq!(a2.name(atom_val(t2)).unwrap(), name);
    }

    // A short multi-byte UTF-8 name.
    {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("héllo"));
        let (b1, t2, _h, a2) = roundtrip(a, &atoms, 64);
        assert_eq!(b1[1], SMALL_ATOM_UTF8_EXT);
        assert_eq!(usize::from(b1[2]), 6, "h, é (2 bytes), l, l, o");
        assert_eq!(a2.name(atom_val(t2)).unwrap(), "héllo");
    }

    // 128 two-byte characters = 128 characters but **256 bytes**, so the one-byte
    // length no longer fits: the wire form must be `ATOM_UTF8_EXT` (118).
    {
        let wide = "é".repeat(128);
        assert_eq!(wide.chars().count(), 128);
        assert_eq!(wide.len(), 256);
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern(&wide));
        let (b1, t2, _h, a2) = roundtrip(a, &atoms, 64);
        assert_eq!(b1[1], ATOM_UTF8_EXT);
        assert_eq!(usize::from(u16::from_be_bytes([b1[2], b1[3]])), 256);
        assert_eq!(a2.name(atom_val(t2)).unwrap(), wide);
    }
}

#[test]
fn all_four_atom_tags_decode() {
    fn image(prefix: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.extend_from_slice(tail);
        v
    }
    let latin1_cafe = [b'c', b'a', b'f', 0xE9];
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (image(&[131, 119, 2], b"ok"), "ok"),    // SMALL_ATOM_UTF8_EXT
        (image(&[131, 118, 0, 2], b"ok"), "ok"), // ATOM_UTF8_EXT
        (image(&[131, 115, 4], &latin1_cafe), "café"), // SMALL_ATOM_EXT (Latin-1)
        (image(&[131, 100, 0, 4], &latin1_cafe), "café"), // ATOM_EXT (Latin-1)
    ];
    for (bytes, name) in &cases {
        let mut h = heap(64);
        let mut a = AtomTable::new();
        let t = decode_term(bytes, &mut h, &mut a).expect("decode");
        assert!(is_atom(t));
        assert_eq!(a.name(atom_val(t)).unwrap(), *name);
        // The canonical re-encoding is always UTF-8.
        assert_eq!(encode_term(t, &a).unwrap()[1], SMALL_ATOM_UTF8_EXT);
    }
}

#[test]
fn latin1_atom_encoding_emits_small_atom_ext() {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("café"));

    // Default UTF-8: `café` is 5 bytes.
    let utf8 = encode_term(a, &atoms).unwrap();
    assert_eq!(utf8[1], SMALL_ATOM_UTF8_EXT);
    assert_eq!(usize::from(utf8[2]), 5);

    // Latin-1: one byte per code point, so 4 bytes and `SMALL_ATOM_EXT` (115).
    let mut enc = Encoder::new(&atoms);
    enc.set_atom_encoding(AtomEncoding::Latin1);
    enc.encode(a).unwrap();
    let latin1 = enc.finish();
    assert_eq!(latin1, vec![131, 115, 4, b'c', b'a', b'f', 0xE9]);

    // Decoding the Latin-1 form yields the same atom name …
    let mut h = heap(64);
    let mut a2 = AtomTable::new();
    let t = decode_term(&latin1, &mut h, &mut a2).unwrap();
    assert!(is_atom(t));
    assert_eq!(a2.name(atom_val(t)).unwrap(), "café");

    // … and re-encoding under Latin-1 is byte-stable.
    let mut enc = Encoder::new(&a2);
    enc.set_atom_encoding(AtomEncoding::Latin1);
    enc.encode(t).unwrap();
    assert_eq!(enc.finish(), latin1);
}

#[test]
fn latin1_encoding_rejects_non_latin1_atoms() {
    let mut atoms = AtomTable::new();
    // U+03BB is outside `U+0000..=U+00FF`, so Latin-1 cannot represent it.
    let a = make_atom(atoms.intern("λ"));

    let mut enc = Encoder::new(&atoms);
    enc.set_atom_encoding(AtomEncoding::Latin1);
    let err = enc.encode(a).unwrap_err();
    assert!(is_unsupported(&err), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

#[test]
fn floats_round_trip_bit_exactly() {
    let atoms = AtomTable::new();
    for &bits in &[
        3.14f64.to_bits(),
        0.0f64.to_bits(),
        (-0.0f64).to_bits(),
        1, // the smallest subnormal
        f64::MIN_POSITIVE.to_bits(),
        f64::MIN.to_bits(),
        f64::MAX.to_bits(),
        0x000f_ffff_ffff_ffff, // the largest subnormal
    ] {
        let mut h1 = heap(16);
        let t = make_float(&mut h1, f64::from_bits(bits)).unwrap();
        let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, 16);
        assert_eq!(b1[1], NEW_FLOAT_EXT);
        assert_eq!(
            float_value(t2).unwrap().to_bits(),
            bits,
            "bits {bits:#018x}"
        );
    }

    // A non-finite value has no `NEW_FLOAT_EXT` image the oracle will accept: the
    // C rejects it at decode time (`if (!erts_isfinite(ff.fd)) goto error;`,
    // `external.c:4667-4669`), so `binary_to_term/1` raises `badarg` for both an
    // infinity and a NaN. Every one of these images must therefore be `BadArg`.
    for &bits in &[
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
        f64::NAN.to_bits(),
        0x7ff0_0000_0000_0001, // signaling NaN
        0x7ff8_0000_dead_beef, // quiet NaN with a payload
        0xfff0_0000_0000_0001, // negative signaling NaN
    ] {
        let mut image = vec![VERSION_MAGIC, NEW_FLOAT_EXT];
        image.extend_from_slice(&bits.to_be_bytes());
        assert_bad_arg(&image);
    }
}

#[test]
fn legacy_float_ext_is_decoded() {
    // `FLOAT_EXT` carries 31 bytes of a NUL-terminated `%.20e` rendering.
    let text = b"1.0e+00";
    let mut image = vec![VERSION_MAGIC, FLOAT_EXT];
    image.extend_from_slice(text);
    image.resize(image.len() + (31 - text.len()), 0);

    let mut h = heap(16);
    let mut a = AtomTable::new();
    let t = decode_term(&image, &mut h, &mut a).unwrap();
    assert_eq!(float_value(t).unwrap().to_bits(), 1.0f64.to_bits());

    // The canonical re-encoding is the modern `NEW_FLOAT_EXT`, not `FLOAT_EXT`.
    assert_eq!(encode_term(t, &a).unwrap()[1], NEW_FLOAT_EXT);
}

// ---------------------------------------------------------------------------
// Tuples
// ---------------------------------------------------------------------------

#[test]
fn tuples_round_trip_at_arity_boundaries() {
    let atoms = AtomTable::new();
    for &arity in &[0usize, 1, 2, 255, 256] {
        let cap = arity + 8;
        let mut h1 = heap(cap);
        let elems: Vec<Eterm> = (0..arity).map(|i| make_small(i as i64)).collect();
        let t = h1.tuple(&elems).unwrap();

        let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, cap);

        assert_eq!(tuple_arity(t2).unwrap(), arity);
        for i in 0..arity {
            assert_eq!(tuple_element(t2, i).unwrap(), make_small(i as i64));
        }
        if arity <= 255 {
            assert_eq!(b1[1], SMALL_TUPLE_EXT);
            assert_eq!(usize::from(b1[2]), arity);
        } else {
            assert_eq!(b1[1], LARGE_TUPLE_EXT);
            assert_eq!(
                u32::from_be_bytes([b1[2], b1[3], b1[4], b1[5]]) as usize,
                arity
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

#[test]
fn byte_lists_use_string_ext_up_to_65535() {
    let atoms = AtomTable::new();

    // The empty list is `[]` → `NIL_EXT`.
    assert_eq!(encode_term(NIL, &atoms).unwrap(), vec![131, NIL_EXT]);
    assert_eq!(encode_term_body(NIL, &atoms).unwrap(), vec![NIL_EXT]);

    // `[1,2,3]` is a proper byte list.
    {
        let mut h1 = heap(32);
        let t = byte_list(&mut h1, &[1, 2, 3]);
        assert_eq!(
            encode_term(t, &atoms).unwrap(),
            vec![131, 107, 0, 3, 1, 2, 3]
        );
    }

    for &len in &[1usize, 255, 256, 65535] {
        let cap = 2 * len + 64;
        let mut h1 = heap(cap);
        let bytes: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
        let t = byte_list(&mut h1, &bytes);

        let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, cap);

        assert_eq!(b1[1], STRING_EXT);
        assert_eq!(usize::from(u16::from_be_bytes([b1[2], b1[3]])), len);
        assert_eq!(&b1[4..], &bytes[..]);

        let (heads, tail) = collect_list(t2);
        assert_eq!(heads.len(), len);
        assert!(is_nil(tail));
        for (i, &head) in heads.iter().enumerate() {
            assert_eq!(head, make_small(i64::from(bytes[i])));
        }
    }
}

#[test]
fn byte_list_of_65536_uses_list_ext() {
    let atoms = AtomTable::new();
    let len = 65536usize;
    let cap = 2 * len + 64;
    let mut h1 = heap(cap);
    let bytes: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
    let t = byte_list(&mut h1, &bytes);

    let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, cap);

    // One past the 16-bit `STRING_EXT` length: the list form is used, and each element
    // is a `SMALL_INTEGER_EXT`.
    assert_eq!(b1[1], LIST_EXT);
    assert_eq!(
        u32::from_be_bytes([b1[2], b1[3], b1[4], b1[5]]) as usize,
        len
    );
    assert_eq!(b1[6], SMALL_INTEGER_EXT);
    assert_eq!(b1[7], bytes[0]);

    let (heads, tail) = collect_list(t2);
    assert_eq!(heads.len(), len);
    assert!(is_nil(tail));
    for (i, &head) in heads.iter().enumerate() {
        assert_eq!(head, make_small(i64::from(bytes[i])));
    }
}

#[test]
fn proper_and_improper_lists_round_trip() {
    let atoms = AtomTable::new();

    // A proper list with a non-byte element: `LIST_EXT` with a `NIL_EXT` tail.
    {
        let mut h1 = heap(64);
        let t = list(&mut h1, &[make_small(256), make_small(300)], NIL);
        let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, 64);
        assert_eq!(
            b1,
            vec![131, 108, 0, 0, 0, 2, 98, 0, 0, 1, 0, 98, 0, 0, 1, 44, 106]
        );
        let (heads, tail) = collect_list(t2);
        assert_eq!(heads, vec![make_small(256), make_small(300)]);
        assert!(is_nil(tail));
    }

    // An improper list `[1, 2 | 3]`.
    {
        let mut h1 = heap(64);
        let t = list(&mut h1, &[make_small(1), make_small(2)], make_small(3));
        let (b1, t2, _h2, _a2) = roundtrip(t, &atoms, 64);
        assert_eq!(b1, vec![131, 108, 0, 0, 0, 2, 97, 1, 97, 2, 97, 3]);
        let (heads, tail) = collect_list(t2);
        assert_eq!(heads, vec![make_small(1), make_small(2)]);
        assert_eq!(tail, make_small(3));
    }
}

// ---------------------------------------------------------------------------
// Maps
// ---------------------------------------------------------------------------

#[test]
fn maps_round_trip_at_pair_boundaries() {
    // A map of at most `MAP_SMALL_MAP_LIMIT` (32) distinct keys is a **flat map**, whose
    // keys the decoder sorts into `erts_cmp_flatmap_keys` order
    // (`beam_term::map::MAP_SMALL_MAP_LIMIT`, `erl_map.c:3187`). The atom keys below are
    // interned in index order, so that sorted order is exactly the insertion order.
    for &n in &[0usize, 1, 2, 32] {
        let mut atoms = AtomTable::new();
        let cap = 4 + 2 * n + 64;
        let mut h1 = heap(cap);

        let mut pairs = Vec::new();
        for i in 0..n {
            let key = make_atom(atoms.intern(&format!("key{i}")));
            pairs.push((key, make_small(i as i64)));
        }
        let m = make_flatmap(&mut h1, &pairs).unwrap();

        let (b1, t2, _h2, a2) = roundtrip(m, &atoms, cap);

        assert_eq!(b1[1], MAP_EXT);
        assert_eq!(u32::from_be_bytes([b1[2], b1[3], b1[4], b1[5]]) as usize, n);

        let (keys, values) = flatmap_parts(t2).unwrap();
        assert_eq!(keys.len(), n);
        assert_eq!(values.len(), n);
        for i in 0..n {
            let expected = format!("key{i}");
            assert!(is_atom(keys[i]));
            assert_eq!(a2.name(atom_val(keys[i])).unwrap(), expected.as_str());
            assert_eq!(values[i], make_small(i as i64));
        }
    }

    // Above `MAP_SMALL_MAP_LIMIT` the map is a **HAMT**, so the canonical `MAP_EXT` image
    // is the HAMT image ERTS emits — not the flat-map order of the input. A
    // `write_map`-built HAMT is byte-identical to the oracle's image
    // (`tests/map_key_order.rs`), so a canonical image round-trips byte-for-byte while a
    // locally built flat map is first *canonicalized* into that HAMT image. Integer keys
    // are used here because a HAMT's storage order depends on `erts_map_hash`, which for
    // an atom depends on the per-VM-start atom index (see `tests/map_key_order.rs`)
    // whereas for an integer it is stable.
    for &n in &[33usize, 40, 100] {
        let mut h1 = heap(4 + 2 * n + 64);
        let pairs: Vec<(Eterm, Eterm)> = (0..n as i64)
            .map(|i| (make_small(i), make_small(i + 100)))
            .collect();
        let flat = make_flatmap(&mut h1, &pairs).unwrap();
        let flat_image = encode_term(flat, &AtomTable::new()).unwrap();
        assert_eq!(flat_image[1], MAP_EXT);
        assert_eq!(
            u32::from_be_bytes([
                flat_image[2],
                flat_image[3],
                flat_image[4],
                flat_image[5]
            ]) as usize,
            n
        );

        // Decoding the (non-canonical) flat image yields a HAMT; re-encoding gives the
        // canonical HAMT image.
        let cap = beam_term::map::map_words_capacity(n) + 64;
        let mut h2 = heap(cap);
        let mut atoms2 = AtomTable::new();
        let t2 = decode_term(&flat_image, &mut h2, &mut atoms2).unwrap();
        assert!(
            flatmap_parts(t2).is_err(),
            "a > 32-pair map decodes to a HAMT, not a flat map"
        );
        assert_eq!(beam_term::map::map_size(t2, &atoms2).unwrap(), n);

        let canonical = encode_term(t2, &atoms2).unwrap();
        assert_eq!(canonical[1], MAP_EXT);
        assert_eq!(
            u32::from_be_bytes([canonical[2], canonical[3], canonical[4], canonical[5]]) as usize,
            n
        );

        // The canonical image is itself byte-stable: decode → encode is the identity.
        let mut h3 = heap(cap);
        let mut atoms3 = AtomTable::new();
        let t3 = decode_term(&canonical, &mut h3, &mut atoms3).unwrap();
        assert_eq!(encode_term(t3, &atoms3).unwrap(), canonical);

        // ... and every key maps to its value (checked independently of representation).
        for i in 0..n as i64 {
            assert_eq!(
                beam_term::map::map_get(t2, make_small(i), &atoms2).unwrap(),
                Some(make_small(i + 100))
            );
        }
    }
}

// ---------------------------------------------------------------------------
// External identifiers
// ---------------------------------------------------------------------------

#[test]
fn external_pids_round_trip() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    for &(number, serial, creation) in &[
        (0u32, 0u32, 0u32),
        (1, 2, 3),
        (u32::MAX, u32::MAX, u32::MAX),
    ] {
        let mut h1 = heap(64);
        let p = make_external_pid(&mut h1, node, number, serial, creation).unwrap();

        let (b1, t2, _h2, a2) = roundtrip(p, &atoms, 64);

        assert_eq!(b1[1], NEW_PID_EXT);
        let (n2, num2, ser2, cre2) = external_pid_parts(t2).unwrap();
        assert!(is_atom(n2));
        assert_eq!(a2.name(atom_val(n2)).unwrap(), "nonode@nohost");
        assert_eq!((num2, ser2, cre2), (number, serial, creation));
    }
}

#[test]
fn external_ports_round_trip_with_the_full_64_bit_id() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    for &(number, creation) in &[
        (0u64, 0u32),
        (0x1234, 5),
        (u64::from(u32::MAX), u32::MAX),
        // Above `NEW_PORT_EXT`'s 32-bit field: the modern `V4_PORT_EXT` writes it in
        // full — never truncated, never a `SystemLimit`.
        (1u64 << 32, 0),
        (1u64 << 40, 7),
        (u64::MAX, u32::MAX),
    ] {
        let mut h1 = heap(64);
        let p = make_external_port(&mut h1, node, number, creation).unwrap();

        let (b1, t2, _h2, a2) = roundtrip(p, &atoms, 64);

        // The encoder always emits `V4_PORT_EXT` (`120`), the form the C emits.
        assert_eq!(b1[1], V4_PORT_EXT);
        assert_eq!(b1[1], 120);
        // Wire: `[120][node atom][id:8 BE][creation:4 BE]`; the node atom here is a
        // `SMALL_ATOM_UTF8_EXT` (`119`) with a 1-byte length at `b1[3]`.
        assert_eq!(b1[2], 119);
        let id_off = 4 + b1[3] as usize; // version + tag + atom tag + len byte + name
        assert_eq!(
            u64::from_be_bytes(b1[id_off..id_off + 8].try_into().unwrap()),
            number
        );
        assert_eq!(
            u32::from_be_bytes(b1[id_off + 8..id_off + 12].try_into().unwrap()),
            creation
        );

        let (n2, num2, cre2) = external_port_parts(t2).unwrap();
        assert_eq!(a2.name(atom_val(n2)).unwrap(), "nonode@nohost");
        assert_eq!((num2, cre2), (number, creation));
    }

    // `NEW_PORT_EXT` (`89`) stays decodable even though the encoder never emits it.
    let mut image = vec![VERSION_MAGIC, NEW_PORT_EXT];
    image.extend_from_slice(&[119, 2, b'n', b'1']);
    image.extend_from_slice(&7u32.to_be_bytes());
    image.extend_from_slice(&9u32.to_be_bytes());
    let mut h = heap(64);
    let mut a = AtomTable::new();
    let t = decode_term(&image, &mut h, &mut a).expect("NEW_PORT_EXT decodes");
    assert_eq!(external_port_parts(t).unwrap().1, 7);
    assert_eq!(external_port_parts(t).unwrap().2, 9);
}

#[test]
fn external_references_round_trip_for_every_id_count() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    for ids_len in 1..=5usize {
        let ids: Vec<u32> = (0..ids_len).map(|i| 0x1000_0000 + i as u32).collect();
        let mut h1 = heap(64);
        let r = make_external_ref(&mut h1, node, &ids, 0xABCD_1234).unwrap();

        let (b1, t2, _h2, a2) = roundtrip(r, &atoms, 64);

        assert_eq!(b1[1], NEWER_REFERENCE_EXT);
        assert_eq!(usize::from(u16::from_be_bytes([b1[2], b1[3]])), ids_len);
        let (n2, ids2, cre2) = external_ref_parts(t2).unwrap();
        assert_eq!(a2.name(atom_val(n2)).unwrap(), "nonode@nohost");
        assert_eq!(ids2, &ids[..]);
        assert_eq!(cre2, 0xABCD_1234);
    }
}

// ---------------------------------------------------------------------------
// Version byte / body form
// ---------------------------------------------------------------------------

#[test]
fn body_form_round_trips_without_the_version_byte() {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("ok"));
    let mut h1 = heap(64);
    let t = h1.tuple(&[a, make_small(17)]).unwrap();

    let body = encode_term_body(t, &atoms).unwrap();
    let with_version = encode_term(t, &atoms).unwrap();
    assert_eq!(body[0], SMALL_TUPLE_EXT, "a body has no version byte");
    assert_eq!(with_version[0], VERSION_MAGIC);
    assert_eq!(&with_version[1..], &body[..]);

    let mut h2 = heap(64);
    let mut a2 = AtomTable::new();
    let mut d = Decoder::new_body(&body);
    let t2 = d.decode(&mut h2, &mut a2).unwrap();
    d.finish().unwrap();
    assert_term_eq(t, &atoms, t2, &a2);

    // `Decoder::new` insists on the version byte that the body form lacks.
    assert!(Decoder::new(&body).is_err());
}

// ---------------------------------------------------------------------------
// COMPRESSED (`80`) — the zlib wrapper
// ---------------------------------------------------------------------------

/// `[VERSION_MAGIC][COMPRESSED_EXT][inner_size:4 BE]` → the declared inner size.
fn declared_inner_size(image: &[u8]) -> usize {
    assert_eq!(image[0], VERSION_MAGIC, "a compressed image starts with 131");
    assert_eq!(image[1], COMPRESSED_EXT, "then the COMPRESSED tag");
    u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize
}

#[test]
fn compressed_image_reverts_to_plain_for_a_small_term() {
    // `{ok,17}` has an 8-byte body; zlib cannot fit it into `8 - 5 == 3` bytes, so
    // `encode_term_compressed` reverts to the plain image — exactly what the C
    // (`external.c:2276-2298`) and the oracle produce:
    // `term_to_binary({ok,17}, [compressed]) == term_to_binary({ok,17})`.
    let mut atoms = AtomTable::new();
    let ok = make_atom(atoms.intern("ok"));
    let mut h = heap(64);
    let term = h.tuple(&[ok, make_small(17)]).unwrap();

    let body = encode_term_body(term, &atoms).unwrap();
    let plain = encode_term(term, &atoms).unwrap();
    for level in 0..=9u8 {
        let image = encode_term_compressed(term, &atoms, level).unwrap();
        assert_eq!(image, plain, "level {level}: a small term must revert");
        assert_eq!(&image[1..], &body[..]);
        // The plain image still decodes back to the original term.
        let mut h2 = heap(64);
        let mut a2 = AtomTable::new();
        let term2 = decode_term(&image, &mut h2, &mut a2).unwrap();
        assert_term_eq(term, &atoms, term2, &a2);
    }
}

#[test]
fn compressed_image_round_trips_a_large_compressible_term() {
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let term = list(&mut h, &[hello; 500], NIL);

    let plain = encode_term(term, &atoms).unwrap();
    let image = encode_term_compressed(term, &atoms, 6).unwrap();
    assert_eq!(declared_inner_size(&image), plain.len() - 1);
    assert!(
        image.len() * 10 < plain.len(),
        "a 500-element list should shrink by more than 10x: {} vs {}",
        image.len(),
        plain.len()
    );

    let mut h2 = heap(4096);
    let mut a2 = AtomTable::new();
    let term2 = decode_term(&image, &mut h2, &mut a2).unwrap();
    assert_term_eq(term, &atoms, term2, &a2);
}

#[test]
fn compressed_body_form_omits_the_version_byte() {
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let term = list(&mut h, &[hello; 500], NIL);

    let body = encode_term_body(term, &atoms).unwrap();
    let image = encode_term_body_compressed(term, &atoms, 6).unwrap();

    // `[COMPRESSED_EXT][inner_size:4 BE][zlib]` — no leading version byte.
    assert_eq!(image[0], COMPRESSED_EXT);
    assert_eq!(
        u32::from_be_bytes([image[1], image[2], image[3], image[4]]) as usize,
        body.len()
    );
    assert_eq!(decompress(&image[5..]).unwrap(), body);

    // Prepending the version byte yields a valid versioned image for the same term.
    let mut versioned = vec![VERSION_MAGIC];
    versioned.extend_from_slice(&image);
    let mut h2 = heap(4096);
    let mut a2 = AtomTable::new();
    let term2 = decode_term(&versioned, &mut h2, &mut a2).unwrap();
    assert_term_eq(term, &atoms, term2, &a2);
}

#[test]
fn compressed_level_out_of_range_is_bad_arg() {
    let atoms = AtomTable::new();
    let term = make_small(7);
    assert!(matches!(
        encode_term_compressed(term, &atoms, 10),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        encode_term_body_compressed(term, &atoms, u8::MAX),
        Err(BeamError::BadArg)
    ));
    // `0` and `9` are the inclusive bounds.
    assert!(encode_term_compressed(term, &atoms, 0).is_ok());
    assert!(encode_term_compressed(term, &atoms, 9).is_ok());
}

#[test]
fn compressed_image_is_a_wrapper_or_a_plain_revert_on_every_level() {
    // A term where compression pays off at most levels, but not necessarily at level
    // 0 (which stores rather than deflates).
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let term = list(&mut h, &[hello; 256], NIL);
    let body = encode_term_body(term, &atoms).unwrap();
    let plain = encode_term(term, &atoms).unwrap();

    for level in 0..=9u8 {
        let image = encode_term_compressed(term, &atoms, level).unwrap();
        if image[1] == COMPRESSED_EXT {
            // The wrapper only appears when the stream fits in `body_len - 5` bytes.
            assert_eq!(declared_inner_size(&image), body.len(), "level {level}");
            assert!(
                image.len() <= plain.len(),
                "level {level}: the wrapper shrinks the image"
            );
            assert_eq!(decompress(&image[6..]).unwrap(), body, "level {level}");
        } else {
            // Reverted: the plain image, byte-for-byte.
            assert_eq!(image, plain, "level {level}: revert must be the plain image");
        }

        let mut h2 = heap(4096);
        let mut a2 = AtomTable::new();
        let term2 = decode_term(&image, &mut h2, &mut a2).unwrap();
        assert_term_eq(term, &atoms, term2, &a2);
    }
}

#[test]
fn compressed_image_negative_cases_are_bad_arg() {
    // A valid image to corrupt.
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let term = list(&mut h, &[hello; 64], NIL);
    let image = encode_term_compressed(term, &atoms, 6).unwrap();

    // A header shorter than 6 bytes (the C only takes the `COMPRESSED` branch once
    // the tag plus the 4-byte size are present).
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT]);
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0]);

    // A declared size that does not match the decompressed length.
    let mut wrong_size = image.clone();
    wrong_size[2..6].copy_from_slice(&0u32.to_be_bytes());
    assert_bad_arg(&wrong_size);

    // A corrupt Adler-32 trailer is rejected by zlib.
    let mut corrupt = image.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert_bad_arg(&corrupt);

    // A payload that is not a zlib stream at all.
    assert_bad_arg(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 4, 1, 2, 3, 4]);

    // A well-formed wrapper whose inner body is not a valid term (200 is not a tag).
    let bad_body = [200u8];
    let zlib = compress(&bad_body, 6).unwrap();
    let mut bad_image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    bad_image.extend_from_slice(&(bad_body.len() as u32).to_be_bytes());
    bad_image.extend_from_slice(&zlib);
    assert_bad_arg(&bad_image);

    // Every truncation past the 6-byte header is rejected — never a panic.
    for len in 6..image.len() {
        assert_bad_arg(&image[..len]);
    }
}

// ---------------------------------------------------------------------------
// Funs — `EXPORT_EXT` (113) / `NEW_FUN_EXT` (112)
// ---------------------------------------------------------------------------

/// The 16-byte MD5 `uniq` used by the hand-built `NEW_FUN_EXT` fixtures (any fixed
/// value works — the wire carries it verbatim).
const UNIQ: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// The `SMALL_ATOM_UTF8_EXT` wire image of a short atom name.
fn small_atom(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    assert!(bytes.len() <= 255, "small_atom is for short names only");
    let mut v = vec![SMALL_ATOM_UTF8_EXT, bytes.len() as u8];
    v.extend_from_slice(bytes);
    v
}

/// A `NEW_PID_EXT` creator-pid image for `nonode@nohost` with id `(1, 2, 3)` — the
/// modern form the encoder always emits (`external.c:3989`). Matches the creator
/// [`imported_fun`] builds, so it can be asserted byte-for-byte.
fn creator_pid_image() -> Vec<u8> {
    let mut v = vec![NEW_PID_EXT];
    v.extend_from_slice(&small_atom("nonode@nohost"));
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(&2u32.to_be_bytes());
    v.extend_from_slice(&3u32.to_be_bytes());
    v
}

/// An `EXPORT_EXT` (`fun lists:reverse/Arity`) term on `h`.
fn external_fun(h: &mut Heap, atoms: &mut AtomTable, arity: usize) -> Eterm {
    let module = atoms.intern("lists");
    let function = atoms.intern("reverse");
    make_fun_external(h, arity, module, function).expect("a fresh heap holds a fun")
}

/// A `NEW_FUN_EXT` fun on `h` with module `ftest`, index `index`, `old_index` 7,
/// `old_uniq` 3 and the fixed creator pid [`creator_pid_image`] describes. `env` is
/// the environment in **wire order** (`env[0]` is stream element 0).
fn imported_fun(h: &mut Heap, atoms: &mut AtomTable, arity: usize, index: u32, env: &[Eterm]) -> Eterm {
    let module = atoms.intern("ftest");
    let creator = FunPid {
        node: atoms.intern("nonode@nohost"),
        number: 1,
        serial: 2,
        creation: 3,
    };
    make_fun_imported(h, arity, module, &UNIQ, index, 7, 3, creator, env)
        .expect("a fresh heap holds a fun")
}

/// Decodes a `term_to_binary` image, re-encodes the decoded term and asserts the
/// re-encoding is **byte-identical** to the input (the wire form is canonical), then
/// decodes the re-encoding once more and checks the two terms are structurally equal.
///
/// Returns the twice-decoded term with the heap/atom-table that own it.
fn roundtrip_image(image: &[u8], cap: usize) -> (Eterm, Heap, AtomTable) {
    let mut h1 = heap(cap);
    let mut a1 = AtomTable::new();
    let t1 = decode_term(image, &mut h1, &mut a1).expect("decode the fixture image");

    let b = encode_term(t1, &a1).expect("re-encode the decoded term");
    assert_eq!(
        b, image,
        "decode → encode must reproduce the wire image byte-for-byte"
    );

    let mut h2 = heap(cap);
    let mut a2 = AtomTable::new();
    let t2 = decode_term(&b, &mut h2, &mut a2).expect("decode the re-encoding");
    assert_term_eq(t1, &a1, t2, &a2);
    (t2, h2, a2)
}

#[test]
fn external_funs_round_trip() {
    // Arity 0 (`fun f/0`), 1 and the 8-bit-header maximum 255.
    for &arity in &[0usize, 1, 255] {
        let mut atoms = AtomTable::new();
        let mut h = heap(64);
        let fun = external_fun(&mut h, &mut atoms, arity);
        let (img, t2, _h2, _a2) = roundtrip(fun, &atoms, 64);

        // `[113][module atom][function atom][arity as a small]`, byte-exact.
        let mut expected = vec![VERSION_MAGIC, EXPORT_EXT];
        expected.extend_from_slice(&small_atom("lists"));
        expected.extend_from_slice(&small_atom("reverse"));
        expected.extend_from_slice(&[SMALL_INTEGER_EXT, arity as u8]);
        assert_eq!(img, expected);

        // The decoded term is the same external fun, with no environment.
        assert_eq!(fun_kind_term(t2).unwrap(), FunKind::External);
        assert_eq!(fun_arity_term(t2).unwrap(), arity);
        assert_eq!(fun_num_free_term(t2).unwrap(), 0);
        assert_eq!(fun_env(t2).unwrap(), &[] as &[Eterm]);
    }

    // A fun whose module atom is longer than 255 *characters* cannot be encoded
    // (`SystemLimit`, exactly like a bare over-long atom).
    let mut atoms = AtomTable::new();
    let module = atoms.intern(&"m".repeat(256));
    let function = atoms.intern("f");
    let mut h = heap(64);
    let fun = make_fun_external(&mut h, 1, module, function).unwrap();
    assert!(matches!(
        encode_term(fun, &atoms),
        Err(BeamError::SystemLimit)
    ));
}

#[test]
fn imported_funs_round_trip() {
    // `num_free` 0, 1 and 3, with distinguishable env values (a small, a list and a
    // binary) so a wrong environment **order** would be caught.
    let mut atoms = AtomTable::new();
    let mut h = heap(512);

    let envs: Vec<Vec<Eterm>> = vec![
        vec![],
        vec![make_small(1)],
        vec![
            make_small(1000),
            list(&mut h, &[make_small(1), make_small(2), make_small(3)], NIL),
            make_binary(&mut h, b"abc").unwrap(),
        ],
    ];

    for (n, env) in envs.iter().enumerate() {
        let fun = imported_fun(&mut h, &mut atoms, n, n as u32, env);
        let (img, t2, _h2, a2) = roundtrip(fun, &atoms, 512);

        // The fixed header: `[112][size:4][arity:1][uniq:16][index:4][num_free:4]`.
        assert_eq!(img[1], NEW_FUN_EXT);
        assert_eq!(img[6], n as u8);
        assert_eq!(img[7..23], UNIQ);
        assert_eq!(img[23..27], (n as u32).to_be_bytes());
        assert_eq!(img[27..31], (env.len() as u32).to_be_bytes());
        // `[module][old_index small][old_uniq small][creator pid NEW_PID_EXT]`.
        let mut head = small_atom("ftest");
        head.extend_from_slice(&[SMALL_INTEGER_EXT, 7]); // old_index
        head.extend_from_slice(&[SMALL_INTEGER_EXT, 3]); // old_uniq
        head.extend_from_slice(&creator_pid_image());
        assert_eq!(&img[31..31 + head.len()], &head[..]);
        // The size field counts from its own first byte through the end of the fun.
        assert_eq!(
            u32::from_be_bytes([img[2], img[3], img[4], img[5]]) as usize,
            img.len() - 2
        );

        // The decoded fun round-trips every descriptor field.
        assert_eq!(fun_kind_term(t2).unwrap(), FunKind::Imported);
        assert_eq!(a2.name(fun_module(t2).unwrap()), Some("ftest"));
        assert_eq!(fun_arity_term(t2).unwrap(), n);
        assert_eq!(fun_num_free_term(t2).unwrap(), env.len());
        assert_eq!(fun_uniq_bytes(t2).unwrap(), UNIQ);
        assert_eq!(fun_index(t2).unwrap(), n as u32);
        assert_eq!(fun_old_index(t2).unwrap(), 7);
        assert_eq!(fun_old_uniq(t2).unwrap(), 3);
        let creator = fun_creator_pid(t2).unwrap();
        assert_eq!(a2.name(creator.node), Some("nonode@nohost"));
        assert_eq!((creator.number, creator.serial, creator.creation), (1, 2, 3));

        // ENV ORDER: `env[k]` is stream element `k`, element by element.
        let decoded = fun_env(t2).unwrap();
        assert_eq!(decoded.len(), env.len());
        for k in 0..env.len() {
            assert_term_eq(env[k], &atoms, decoded[k], &a2);
        }
    }
}

#[test]
fn nested_funs_round_trip() {
    let mut atoms = AtomTable::new();
    let mut h = heap(1024);

    let ext = external_fun(&mut h, &mut atoms, 1);
    let imp = imported_fun(&mut h, &mut atoms, 0, 9, &[make_small(42)]);

    // A fun inside a tuple.
    let t = h.tuple(&[make_small(1), ext]).unwrap();
    let (img, _t2, _h2, _a2) = roundtrip(t, &atoms, 1024);
    assert_eq!(img[1], SMALL_TUPLE_EXT);

    // A fun inside a list.
    let l = list(&mut h, &[make_small(2), imp], NIL);
    roundtrip(l, &atoms, 1024);

    // A fun as a map value.
    let key = make_atom(atoms.intern("k"));
    let m = make_flatmap(&mut h, &[(key, imp)]).unwrap();
    roundtrip(m, &atoms, 1024);

    // A fun inside a tuple, itself inside a list.
    let inner = h.tuple(&[ext, imp]).unwrap();
    let outer = list(&mut h, &[make_small(3), inner], NIL);
    let (img, outer2, _h2, _a2) = roundtrip(outer, &atoms, 1024);
    // `[108 LIST_EXT][arity:4][head][tail]` with the head a 2-tuple.
    assert_eq!(img[1], LIST_EXT);
    let (heads, tail) = collect_list(outer2);
    assert!(is_nil(tail));
    assert_eq!(heads.len(), 2);
    assert_eq!(tuple_arity(heads[1]).unwrap(), 2);
    assert_eq!(fun_kind_term(tuple_element(heads[1], 0).unwrap()).unwrap(), FunKind::External);
    assert_eq!(fun_kind_term(tuple_element(heads[1], 1).unwrap()).unwrap(), FunKind::Imported);
}

#[test]
fn funs_round_trip_mixed_with_other_tags() {
    // One enclosing tuple carrying a fun alongside floats, bignums, bitstrings, atoms
    // and strings — the fun tags must route through the shared walker.
    let mut atoms = AtomTable::new();
    let mut h = heap(1024);

    let float = make_float(&mut h, 3.5).unwrap();
    let big = make_big(&mut h, false, &[0, 1]).unwrap(); // 2^64 ⇒ a bignum
    let bin = make_binary(&mut h, b"hello").unwrap();
    let atom = make_atom(atoms.intern("an_atom"));
    let str_list = byte_list(&mut h, b"xyz");
    let ext = external_fun(&mut h, &mut atoms, 2);
    let imp = imported_fun(&mut h, &mut atoms, 1, 5, &[make_small(7)]);

    let t = h
        .tuple(&[float, big, bin, atom, str_list, ext, imp])
        .unwrap();
    let (img, t2, _h2, _a2) = roundtrip(t, &atoms, 1024);

    assert_eq!(img[1], SMALL_TUPLE_EXT);
    assert_eq!(img[2], 7);
    assert_eq!(tuple_arity(t2).unwrap(), 7);
    assert_eq!(fun_kind_term(tuple_element(t2, 5).unwrap()).unwrap(), FunKind::External);
    assert_eq!(fun_kind_term(tuple_element(t2, 6).unwrap()).unwrap(), FunKind::Imported);
}

/// Byte-exact oracle vector for an `EXPORT_EXT` fun, generated with:
/// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(fun lists:reverse/1)])' -s init stop`.
const ORACLE_EXPORT_FUN: &[u8] = &[
    131, 113, 119, 5, 108, 105, 115, 116, 115, 119, 7, 114, 101, 118, 101, 114, 115, 101, 97, 1,
];

/// Byte-exact oracle vector for a genuine `NEW_FUN_EXT`, from a helper module whose
/// source is:
/// ```erlang
/// -module(ftest).
/// -export([make3/3]).
/// make3(A,B,C) -> fun() -> {A,B,C} end.
/// ```
/// compiled with `erlc ftest.erl` and generated with:
/// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(ftest:make3("aa","bb","cc"))])' -s init stop`.
/// It is `NEW_FUN_EXT`, size 86, arity 0, module `ftest`, index 0, `num_free` 3, the
/// `old_index` a `SMALL_INTEGER_EXT` 0 and the `old_uniq` an `INTEGER_EXT` `0x056D5F58`,
/// with the environment `["aa","bb","cc"]` **in that wire order**.
///
/// The generator's node atom is the default `nonode@nohost` (13 characters); the
/// vector below spells its `SMALL_ATOM_UTF8_EXT` payload in full. Verified against the
/// system oracle: `binary_to_term/1` accepts it (`#Fun<ftest.0.91053912>`) and
/// `term_to_binary/1` reproduces these exact bytes.
const ORACLE_NEW_FUN: &[u8] = &[
    131, 112, 0, 0, 0, 86, 0, 173, 171, 235, 30, 156, 214, 196, 144, 67, 13, 207, 141, 193, 52,
    206, 33, 0, 0, 0, 0, 0, 0, 0, 3, 119, 5, 102, 116, 101, 115, 116, 97, 0, 98, 5, 109, 95, 88,
    88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 107, 0, 2, 97, 97, 107, 0, 2, 98, 98, 107, 0, 2, 99, 99,
];

#[test]
fn oracle_external_fun_image_round_trips() {
    // Decode → re-encode reproduces the oracle bytes; the decoded term is asserted
    // through the public accessors.
    let (t2, _h2, a2) = roundtrip_image(ORACLE_EXPORT_FUN, 64);
    assert_eq!(fun_kind_term(t2).unwrap(), FunKind::External);
    assert_eq!(a2.name(fun_module(t2).unwrap()), Some("lists"));
    assert_eq!(a2.name(fun_function(t2).unwrap()), Some("reverse"));
    assert_eq!(fun_arity_term(t2).unwrap(), 1);
    assert_eq!(fun_num_free_term(t2).unwrap(), 0);
    assert_eq!(fun_env(t2).unwrap(), &[] as &[Eterm]);
}

#[test]
fn oracle_new_fun_image_round_trips() {
    let (t2, _h2, a2) = roundtrip_image(ORACLE_NEW_FUN, 256);

    assert_eq!(fun_kind_term(t2).unwrap(), FunKind::Imported);
    assert_eq!(a2.name(fun_module(t2).unwrap()), Some("ftest"));
    assert_eq!(fun_arity_term(t2).unwrap(), 0);
    assert_eq!(fun_num_free_term(t2).unwrap(), 3);
    assert_eq!(fun_index(t2).unwrap(), 0);
    assert_eq!(fun_old_index(t2).unwrap(), 0);
    assert_eq!(fun_old_uniq(t2).unwrap(), 0x056D_5F58);
    assert_eq!(
        fun_uniq_bytes(t2).unwrap(),
        [173, 171, 235, 30, 156, 214, 196, 144, 67, 13, 207, 141, 193, 52, 206, 33]
    );

    let creator = fun_creator_pid(t2).unwrap();
    assert_eq!(a2.name(creator.node), Some("nonode@nohost"));
    assert_eq!((creator.number, creator.serial, creator.creation), (0, 0, 0));

    // The environment is `["aa","bb","cc"]` **in that order** (a reversed decode
    // would put `"cc"` first).
    let env = fun_env(t2).unwrap();
    assert_eq!(env.len(), 3);
    for (i, want) in ["aa", "bb", "cc"].iter().enumerate() {
        let (heads, tail) = collect_list(env[i]);
        assert!(is_nil(tail), "env[{i}] must be a proper list");
        let bytes: Vec<u8> = heads.iter().map(|&c| signed_value(c) as u8).collect();
        assert_eq!(bytes, want.as_bytes(), "env[{i}] order mismatch");
    }
}

/// Byte-exact oracle vector for the `erl_eval` lambda `fun() -> ok end` — the exact
/// blob the differential case `wire/new_fun_ext_encode` compares. Generated with:
/// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(fun() -> ok end)])' -s init stop`.
///
/// It is `NEW_FUN_EXT` (`112`), total size 136, `arity` 0, module `erl_eval`,
/// `index` 43, `num_free` 1, `old_index` a `SMALL_INTEGER_EXT` 43 and `old_uniq` an
/// `INTEGER_EXT` `0x07C1297F` (130099583). The single environment element is
/// `erl_eval`'s free-variable tree and the creator pid is the default
/// `nonode@nohost` node (`0`/`0`/`0`).
const ORACLE_NEW_FUN_EVAL: &[u8] = &[
    131, 112, 0, 0, 0, 136, 0, 248, 37, 47, 246, 123, 244, 211, 6, 126, 138, 154, 65, 205, 61, 207,
    38, 0, 0, 0, 43, 0, 0, 0, 1, 119, 8, 101, 114, 108, 95, 101, 118, 97, 108, 97, 43, 98, 7, 193,
    41, 127, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 104, 6, 97, 1, 106, 119, 4, 110, 111, 110, 101, 119, 4, 110, 111,
    110, 101, 116, 0, 0, 0, 0, 108, 0, 0, 0, 1, 104, 5, 119, 6, 99, 108, 97, 117, 115, 101, 97, 1,
    106, 106, 108, 0, 0, 0, 1, 104, 3, 119, 4, 97, 116, 111, 109, 97, 1, 119, 2, 111, 107, 106, 106,
];

#[test]
fn oracle_eval_new_fun_image_round_trips() {
    // `roundtrip_image` decodes the blob, re-encodes the decoded term and asserts the
    // re-encoding equals the input **byte-for-byte** (then decodes once more and checks
    // structural equality). An offline golden test: it never needs `erl`.
    let (t2, _h2, a2) = roundtrip_image(ORACLE_NEW_FUN_EVAL, 1024);

    assert_eq!(fun_kind_term(t2).unwrap(), FunKind::Imported);
    assert_eq!(a2.name(fun_module(t2).unwrap()), Some("erl_eval"));
    assert_eq!(fun_arity_term(t2).unwrap(), 0);
    assert_eq!(fun_num_free_term(t2).unwrap(), 1);
    assert_eq!(fun_index(t2).unwrap(), 43);
    assert_eq!(fun_old_index(t2).unwrap(), 43);
    assert_eq!(fun_old_uniq(t2).unwrap(), 0x07C1_297F);
    assert_eq!(
        fun_uniq_bytes(t2).unwrap(),
        [248, 37, 47, 246, 123, 244, 211, 6, 126, 138, 154, 65, 205, 61, 207, 38]
    );

    let creator = fun_creator_pid(t2).unwrap();
    assert_eq!(a2.name(creator.node), Some("nonode@nohost"));
    assert_eq!((creator.number, creator.serial, creator.creation), (0, 0, 0));

    // The one environment slot is `erl_eval`'s free-variable tree.
    assert_eq!(fun_env(t2).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Negative cases
// ---------------------------------------------------------------------------

/// Decodes `bytes` and asserts the error matches `pred`.
fn decode_err(bytes: &[u8]) -> BeamError {
    let mut h = heap(4096);
    let mut a = AtomTable::new();
    decode_term(bytes, &mut h, &mut a).expect_err("malformed input must yield an error")
}

fn assert_bad_arg(bytes: &[u8]) {
    let err = decode_err(bytes);
    assert!(
        matches!(err, BeamError::BadArg),
        "expected BadArg, got {err:?}"
    );
}

fn assert_system_limit(bytes: &[u8]) {
    let err = decode_err(bytes);
    assert!(
        matches!(err, BeamError::SystemLimit),
        "expected SystemLimit, got {err:?}"
    );
}

#[test]
fn truncation_at_every_offset_is_bad_arg() {
    // A `NEW_PID_EXT` image built here rather than pasted, to keep the byte offsets
    // honest.
    let mut pid_image = vec![VERSION_MAGIC, NEW_PID_EXT];
    pid_image.extend_from_slice(&[SMALL_ATOM_UTF8_EXT, 13]);
    pid_image.extend_from_slice(b"nonode@nohost");
    pid_image.extend_from_slice(&1u32.to_be_bytes());
    pid_image.extend_from_slice(&2u32.to_be_bytes());
    pid_image.extend_from_slice(&3u32.to_be_bytes());

    let vectors: Vec<Vec<u8>> = vec![
        vec![131, 104, 2, 119, 2, 111, 107, 97, 17],   // {ok, 17}
        vec![131, 107, 0, 3, 1, 2, 3],                 // [1, 2, 3]
        vec![131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1], // #{a => 1}
        vec![131, 70, 64, 9, 30, 184, 81, 235, 133, 31], // 3.14
        pid_image,
    ];

    for image in &vectors {
        for len in 0..image.len() {
            assert_bad_arg(&image[..len]);
        }
    }
}

#[test]
fn unknown_tag_bytes_are_bad_arg() {
    // `VERSION_MAGIC` itself is not a term tag, and 0/200/255 are not tags at all.
    for &tag in &[0u8, 200, 255, 131] {
        assert_bad_arg(&[VERSION_MAGIC, tag]);
    }
    // A body decoder treats a stray version byte the same way.
    let mut h = heap(64);
    let mut a = AtomTable::new();
    let mut d = Decoder::new_body(&[VERSION_MAGIC, 97, 17]);
    let err = d.decode(&mut h, &mut a).unwrap_err();
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn obsolete_tags_are_bad_arg_and_unimplemented_tags_are_unsupported() {
    // `FUN_EXT` (removed in OTP 23) and `NEW_CACHE` (dead, `#if 0`) are malformed: the
    // C decoder also rejects them outright; they are never `unsupported`.
    assert_bad_arg(&[VERSION_MAGIC, 117]); // FUN_EXT
    assert_bad_arg(&[VERSION_MAGIC, 78]); // NEW_CACHE

    // `RECORD_EXT` (67) is implemented now, so a bare tag byte is a *truncation* (the
    // count, flags and fields are missing) — `BadArg`, never a wrong term.
    assert_bad_arg(&[VERSION_MAGIC, 67]); // RECORD_EXT

    // `LOCAL_EXT` (121) is honoured now as the `term_to_binary/2 [local]` wrapper
    // `[VERSION_MAGIC][LOCAL_EXT][node-hash:4][term]`, so a bare tag byte is a
    // *truncation* — `BadArg`, never `unsupported`.
    assert_bad_arg(&[VERSION_MAGIC, 121]); // LOCAL_EXT

    // `DIST_HEADER` (68) is a distribution-transport wrapper, never a term: the C term
    // decoder rejects it as malformed, so it is `BadArg`, never `unsupported`.
    assert_bad_arg(&[VERSION_MAGIC, 68]); // DIST_HEADER

    // `NEW_FUN_EXT` (112) and `EXPORT_EXT` (113) are implemented now, so a bare tag
    // byte is a *truncation* — `BadArg` (the C also errors running off the end) —
    // never `unsupported`.
    assert_bad_arg(&[VERSION_MAGIC, 112]); // NEW_FUN_EXT
    assert_bad_arg(&[VERSION_MAGIC, 113]); // EXPORT_EXT
}

#[test]
fn local_ext_wrapper_decodes_the_next_term_and_dist_header_is_bad_arg() {
    // A hand-built `term_to_binary({ok,17}, [local])`:
    // `[VERSION_MAGIC][LOCAL_EXT][node-hash:4 BE][<term>]`. The 4-byte node hash is
    // opaque here (verification needs the node's `local_node_hash`) and is skipped.
    let mut image = vec![VERSION_MAGIC, 121, 0x12, 0x34, 0x56, 0x78];
    image.extend_from_slice(&[104, 2, 119, 2, 111, 107, 97, 17]); // {ok,17}

    let mut h = heap(64);
    let mut a = AtomTable::new();
    let term = decode_term(&image, &mut h, &mut a).unwrap();
    assert_eq!(tuple_arity(term).unwrap(), 2);
    assert_eq!(
        a.name(atom_val(tuple_element(term, 0).unwrap())),
        Some("ok")
    );
    assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));

    // A `LOCAL_EXT` missing its 4-byte node hash is a truncation.
    assert_bad_arg(&[VERSION_MAGIC, 121, 0, 0, 0]);

    // `DIST_HEADER` is never a term: the framing is `beam_dist`'s, and the term
    // decoder rejects the tag as malformed.
    assert_bad_arg(&[VERSION_MAGIC, 68]);
    assert_bad_arg(&[VERSION_MAGIC, 68, 0, 0, 0, 1, 131]);
}

#[test]
fn export_fun_decode_negative_cases_are_typed_errors() {
    let m = small_atom("m");
    let f = small_atom("f");

    // Truncated right after the tag — the module atom is entirely absent.
    assert_bad_arg(&[VERSION_MAGIC, EXPORT_EXT]);

    // The module is not an atom (a small integer here).
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]);
    assert_bad_arg(&img);

    // The function is not an atom.
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&m);
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]);
    assert_bad_arg(&img);

    // The arity term is absent.
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&m);
    img.extend_from_slice(&f);
    assert_bad_arg(&img);

    // The arity is not a small: an atom, an (empty) tuple or a bignum.
    for bad in [
        small_atom("a"),
        vec![SMALL_TUPLE_EXT, 0],
        vec![SMALL_BIG_EXT, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1], // 2^64 ⇒ a bignum
    ] {
        let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
        img.extend_from_slice(&m);
        img.extend_from_slice(&f);
        img.extend_from_slice(&bad);
        assert_bad_arg(&img);
    }

    // A negative arity ⇒ `BadArg` (the C requires `>= 0`).
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&m);
    img.extend_from_slice(&f);
    img.extend_from_slice(&[INTEGER_EXT, 0xFF, 0xFF, 0xFF, 0xFF]); // -1
    assert_bad_arg(&img);

    // An arity past the header's 8-bit field ⇒ `SystemLimit`.
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&m);
    img.extend_from_slice(&f);
    img.extend_from_slice(&[INTEGER_EXT, 0, 0, 1, 0]); // 256
    assert_system_limit(&img);

    // Trailing garbage after a complete `EXPORT_EXT` fun ⇒ `BadArg`.
    let mut img = vec![VERSION_MAGIC, EXPORT_EXT];
    img.extend_from_slice(&m);
    img.extend_from_slice(&f);
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 1]);
    img.push(0);
    assert_bad_arg(&img);
}

/// The fixed `NEW_FUN_EXT` header through the `num_free` field:
/// `[tag][size:4][arity:1][uniq:16][index:4][num_free:4]`. The `size` field is a
/// placeholder — the decoder reads and ignores it (`external.c:5115`).
fn new_fun_header(arity: u8, num_free: u32) -> Vec<u8> {
    let mut v = vec![VERSION_MAGIC, NEW_FUN_EXT];
    v.extend_from_slice(&0u32.to_be_bytes());
    v.push(arity);
    v.extend_from_slice(&UNIQ);
    v.extend_from_slice(&0u32.to_be_bytes()); // index
    v.extend_from_slice(&num_free.to_be_bytes());
    v
}

/// A complete, valid `NEW_FUN_EXT` whose environment is `env` (each a small value).
fn new_fun_image(arity: u8, env: &[u8]) -> Vec<u8> {
    let mut v = new_fun_header(arity, env.len() as u32);
    v.extend_from_slice(&small_atom("ftest")); // module
    v.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index
    v.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_uniq
    v.extend_from_slice(&creator_pid_image());
    for &e in env {
        v.extend_from_slice(&[SMALL_INTEGER_EXT, e]);
    }
    v
}

#[test]
fn new_fun_truncation_at_every_field_boundary_is_bad_arg() {
    // A valid hand-built image: truncating it at *any* offset — the tag, the 4-byte
    // size, the arity, mid-`uniq`, the index, `num_free`, mid-module, the creator pid
    // or the environment — must be `BadArg`, never a panic.
    let image = new_fun_image(1, &[1, 2, 3]);
    for len in 0..image.len() {
        assert_bad_arg(&image[..len]);
    }
    // The oracle image truncates the same way.
    for len in 0..ORACLE_NEW_FUN.len() {
        assert_bad_arg(&ORACLE_NEW_FUN[..len]);
    }
}

#[test]
fn new_fun_field_violations_are_typed_errors() {
    // `num_free` past the 8-bit header field ⇒ `BadArg` (never a corrupt header). The
    // decoder rejects it before reading the module atom, so the header alone suffices.
    assert_bad_arg(&new_fun_header(0, 256));

    // The module must be an atom.
    let mut img = new_fun_header(0, 0);
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]);
    assert_bad_arg(&img);

    // `old_index` must be a small: an atom, an (empty) tuple or a bignum.
    for bad in [
        small_atom("a"),
        vec![SMALL_TUPLE_EXT, 0],
        vec![SMALL_BIG_EXT, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    ] {
        let mut img = new_fun_header(0, 0);
        img.extend_from_slice(&small_atom("m"));
        img.extend_from_slice(&bad);
        assert_bad_arg(&img);
    }

    // `old_uniq` must be a small too.
    for bad in [
        small_atom("a"),
        vec![SMALL_TUPLE_EXT, 0],
        vec![SMALL_BIG_EXT, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    ] {
        let mut img = new_fun_header(0, 0);
        img.extend_from_slice(&small_atom("m"));
        img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index
        img.extend_from_slice(&bad); // old_uniq
        assert_bad_arg(&img);
    }

    // The creator term must be a pid: a reference, a port, a `LOCAL_EXT` (`121`, a
    // wrapper rather than a pid), an atom and a tuple are all rejected (the C accepts
    // only `is_pid`, and this codec reads the creator pid straight off the wire).
    for bad in [
        vec![NEWER_REFERENCE_EXT], // 90
        vec![NEW_PORT_EXT],        // 89
        vec![V4_PORT_EXT],         // 120
        vec![121],                 // LOCAL_EXT
        small_atom("a"),
        vec![SMALL_TUPLE_EXT, 0],
    ] {
        let mut img = new_fun_header(0, 0);
        img.extend_from_slice(&small_atom("m"));
        img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index
        img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_uniq
        img.extend_from_slice(&bad); // creator
        assert_bad_arg(&img);
    }

    // Fewer environment terms than `num_free` ⇒ `BadArg` (a truncated env).
    let mut img = new_fun_header(0, 2);
    img.extend_from_slice(&small_atom("m"));
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_uniq
    img.extend_from_slice(&creator_pid_image());
    img.extend_from_slice(&[SMALL_INTEGER_EXT, 7]); // env[0] only — env[1] is missing
    assert_bad_arg(&img);

    // Trailing garbage after a complete fun ⇒ `BadArg` (`finish` rejects it).
    let mut img = new_fun_image(0, &[]);
    img.push(0);
    assert_bad_arg(&img);
}

#[test]
fn obsolete_fun_and_cache_tags_are_bad_arg() {
    // `FUN_EXT` (117) was removed in OTP 23 and `NEW_CACHE` (78) is dead (`#if 0`);
    // the C rejects both outright — with or without a payload — and they are never
    // `unsupported`.
    assert_bad_arg(&[VERSION_MAGIC, 117]);
    assert_bad_arg(&[VERSION_MAGIC, 117, 0, 0, 0, 0]);
    assert_bad_arg(&[VERSION_MAGIC, 78]);
    assert_bad_arg(&[VERSION_MAGIC, 78, 0, 0]);
}

#[test]
fn encoding_a_local_fun_is_unsupported_local_fun() {
    // A local (`make_fun3`) closure has no faithful `NEW_FUN_EXT` image (no module, no
    // 16-byte `uniq`, no creator pid). The codec's test-only `make_fun_local` builder
    // is not visible from an integration test, so build the identical 3-word block
    // `[header, uniq, index]` from the public `beam_term` API: the header from
    // `make_fun_header` plus `make_boxed` over the two descriptor words. The `Vec`
    // owns the words and stays alive for the whole test.
    let words = vec![
        make_fun_header(1, 0, FunKind::Local).unwrap(),
        Eterm::from_raw(0x42), // uniq
        Eterm::from_raw(5),    // index
    ];
    let local = make_boxed(words.as_ptr());

    let atoms = AtomTable::new();
    let err = encode_term(local, &atoms).expect_err("a local fun has no wire image");
    assert!(
        is_unsupported_local_fun(&err),
        "expected unsupported_local_fun, got {err:?}"
    );
    // It is reported through its own reason atom, distinct from the general
    // "recognized tag not implemented" signal.
    assert!(
        !is_unsupported(&err),
        "a local fun is not the generic unsupported tag"
    );
    assert_eq!(beam_codec::LOCAL_FUN_REASON, "unsupported_local_fun");

    // Reaching a local fun nested in another term fails the same way.
    let mut h = heap(64);
    let t = h.tuple(&[local]).unwrap();
    let err = encode_term(t, &atoms).expect_err("a nested local fun has no wire image");
    assert!(is_unsupported_local_fun(&err), "got {err:?}");
}

#[test]
fn binaries_and_bitstrings_round_trip() {
    // Byte-exact oracle images: `<<"hi">>` (BINARY_EXT) and `<<1:3>>` (BIT_BINARY_EXT).
    let mut heap1 = heap(64);
    let mut atoms1 = AtomTable::new();
    let t = decode_term(&[131, 109, 0, 0, 0, 2, b'h', b'i'], &mut heap1, &mut atoms1).unwrap();
    let (_b, _t2, _h, _a) = roundtrip(t, &atoms1, 64);

    let mut heap2 = heap(64);
    let mut atoms2 = AtomTable::new();
    let t = decode_term(&[131, 77, 0, 0, 0, 1, 3, 0b0010_0000], &mut heap2, &mut atoms2).unwrap();
    let (_b, _t2, _h, _a) = roundtrip(t, &atoms2, 64);

    // The bare `BINARY_EXT` tag with a lying length is BadArg, not unsupported.
    assert_bad_arg(&[VERSION_MAGIC, BINARY_EXT, 0, 0, 0, 5, b'h']);
}

#[test]
fn over_long_atoms_are_bad_arg() {
    // A crafted `ATOM_UTF8_EXT` carrying 256 characters. Decoding it is malformed
    // input (`badarg`), matching the oracle: C rejects an over-long name in
    // `erts_atom_put`/`erts_atom_get` (`atom.c:258-274`, `atom.c:392-418`) and
    // `dec_term` turns that into `goto error` (`external.c:4529-4539`).
    let mut image = vec![VERSION_MAGIC, ATOM_UTF8_EXT, 0x01, 0x00];
    image.extend(std::iter::repeat(b'a').take(256));
    assert_bad_arg(&image);

    // Latin-1 too.
    let mut latin1 = vec![VERSION_MAGIC, ATOM_EXT, 0x01, 0x00];
    latin1.extend(std::iter::repeat(b'a').take(256));
    assert_bad_arg(&latin1);

    // Encoding a 256-character name is still a `SystemLimit` (the encoder's own
    // bound; the wire has no way to carry it).
    let mut atoms = AtomTable::new();
    let long = make_atom(atoms.intern(&"a".repeat(256)));
    assert!(matches!(
        encode_term(long, &atoms),
        Err(BeamError::SystemLimit)
    ));
}

#[test]
fn oversized_bignums_are_bad_arg() {
    // `LARGE_BIG_EXT` with a byte count past `BIG_ARITY_MAX * sizeof(ErtsDigit)`
    // (65535 * 8 = 524280 bytes, `external.c:6146`). The bound is a wire bound, and
    // the C routes every decode failure — bounds included — to `B2TBadArg`
    // (`external.c:1989-1991`), so the oracle answers `badarg`, not `system_limit`.
    assert_bad_arg(&[VERSION_MAGIC, LARGE_BIG_EXT, 0x00, 0x08, 0x00, 0x19]);

    // Building such a bignum is rejected by the constructor as well (the bound is on
    // the digit count).
    let mut h1 = heap(64);
    let digits = vec![1u64; 65536];
    assert!(matches!(
        make_big(&mut h1, false, &digits),
        Err(BeamError::SystemLimit)
    ));
    // … and a zero-digit magnitude is malformed.
    assert!(matches!(
        make_big(&mut h1, false, &[]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn non_zero_bignum_sign_byte_means_negative() {
    /// Decodes one image and returns the term plus the heap/atom table that own it.
    fn decode(image: &[u8], cap: usize) -> (Eterm, Heap, AtomTable) {
        let mut h = heap(cap);
        let mut a = AtomTable::new();
        let t = decode_term(image, &mut h, &mut a).expect("decode the fixture image");
        (t, h, a)
    }

    // C reads `neg = get_int8(ep)` and treats it as a boolean: **any** non-zero
    // sign byte means negative (`if (xsgn) d = -d;`, `big.c:3446-3448`), so the
    // oracle's `binary_to_term(<<131,110,1,2,42>>)` is `-42`, not `badarg`.
    let (t, _h, _a) = decode(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 2, 42], 64);
    assert_eq!(t, make_small(-42));
    assert!(is_small(t));

    // Sign byte 1 is the canonical negative form.
    let (t, _h, _a) = decode(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 1, 42], 64);
    assert_eq!(t, make_small(-42));

    // Sign byte 0 is positive.
    let (t, _h, _a) = decode(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 0, 42], 64);
    assert_eq!(t, make_small(42));

    // A zero magnitude is zero whatever the sign byte is
    // (`external.c:4484-4486` "Zero width bignum defaults to zero").
    let (t, _h, _a) = decode(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 5, 0], 64);
    assert_eq!(t, make_small(0));

    // The same rule applies to a magnitude that becomes a bignum: 2^64 (nine
    // base-256 bytes) with sign byte 200 is negative, and its digits are the
    // magnitude's.
    let mut image = vec![VERSION_MAGIC, SMALL_BIG_EXT, 9, 200];
    image.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let (t, _h, _a) = decode(&image, 64);
    let (negative, digits) = big_parts(t).unwrap();
    assert!(negative);
    assert_eq!(digits, &[0, 1]);
}

#[test]
fn maps_with_more_than_32_pairs_round_trip() {
    // 40 pairs is above `MAP_SMALL_MAP_LIMIT`: the decoded map is a **HAMT**, so the
    // canonical `MAP_EXT` image is the HAMT image ERTS emits (not the flat-map order of
    // the input). A `write_map`-built HAMT is byte-identical to the oracle's image
    // (`tests/map_key_order.rs`), so once canonicalized the wire form is byte-stable.
    // Integer keys keep the HAMT deterministic (an atom key's hash depends on the
    // per-VM-start atom index).
    let n = 40usize;
    let mut h1 = heap(4 + 2 * n + 64);
    let pairs: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(i + 100)))
        .collect();
    let m = make_flatmap(&mut h1, &pairs).unwrap();

    let atoms = AtomTable::new();
    let flat_image = encode_term(m, &atoms).unwrap();
    assert_eq!(flat_image[1], MAP_EXT);
    assert_eq!(
        u32::from_be_bytes([flat_image[2], flat_image[3], flat_image[4], flat_image[5]]),
        40
    );

    // Decode → HAMT; the flat path no longer applies.
    let cap = beam_term::map::map_words_capacity(n) + 64;
    let mut h2 = heap(cap);
    let mut atoms2 = AtomTable::new();
    let t2 = decode_term(&flat_image, &mut h2, &mut atoms2).unwrap();
    assert!(
        flatmap_parts(t2).is_err(),
        "a 40-pair map is a HAMT, not a flat map"
    );
    assert_eq!(beam_term::map::map_size(t2, &atoms2).unwrap(), 40);

    // The canonical HAMT image round-trips byte-for-byte, and holds every pair.
    let canonical = encode_term(t2, &atoms2).unwrap();
    assert_eq!(canonical[1], MAP_EXT);
    assert_eq!(
        u32::from_be_bytes([canonical[2], canonical[3], canonical[4], canonical[5]]),
        40
    );
    let mut h3 = heap(cap);
    let mut atoms3 = AtomTable::new();
    let t3 = decode_term(&canonical, &mut h3, &mut atoms3).unwrap();
    assert_eq!(encode_term(t3, &atoms3).unwrap(), canonical);
    for i in 0..n as i64 {
        assert_eq!(
            beam_term::map::map_get(t2, make_small(i), &atoms2).unwrap(),
            Some(make_small(i + 100))
        );
    }

    // A pair count whose bit 31 is set is malformed (`external.c:6312`).
    assert_bad_arg(&[VERSION_MAGIC, MAP_EXT, 0x80, 0x00, 0x00, 0x00]);
    // A pair count that overruns the wire is `BadArg`: the old flatmap-`arityval`
    // `SystemLimit` guard is gone, since a HAMT has no `MAX_ARITYVAL` bound.
    assert_bad_arg(&[VERSION_MAGIC, MAP_EXT, 0x01, 0x00, 0x00, 0x00]);
}

#[test]
fn trailing_bytes_are_rejected() {
    let image = [VERSION_MAGIC, 97, 17, 0];
    assert_bad_arg(&image);

    let mut h = heap(64);
    let mut a = AtomTable::new();
    let mut d = Decoder::new(&image).unwrap();
    let term = d.decode(&mut h, &mut a).unwrap();
    assert_eq!(term, make_small(17));
    assert!(matches!(d.finish(), Err(BeamError::BadArg)));
}

#[test]
fn malformed_inputs_never_panic() {
    // Every possible tag byte after the version byte, with no payload.
    let mut corpus: Vec<Vec<u8>> = (0u16..=255).map(|t| vec![VERSION_MAGIC, t as u8]).collect();
    // Length fields that lie about how many bytes follow.
    corpus.extend([
        vec![VERSION_MAGIC, LIST_EXT, 255, 255, 255, 255],
        vec![VERSION_MAGIC, LARGE_TUPLE_EXT, 255, 255, 255, 255],
        vec![VERSION_MAGIC, STRING_EXT, 255, 255],
        vec![VERSION_MAGIC, ATOM_EXT, 255, 255],
        vec![VERSION_MAGIC, NEW_REFERENCE_EXT, 255, 255, 119, 1, 97],
        vec![VERSION_MAGIC, NEWER_REFERENCE_EXT, 255, 255, 119, 1, 97],
        vec![VERSION_MAGIC, MAP_EXT, 0, 0, 0, 1],
        vec![VERSION_MAGIC, SMALL_TUPLE_EXT, 4, 97],
        vec![VERSION_MAGIC, LARGE_BIG_EXT, 0, 0, 1, 0],
        vec![VERSION_MAGIC, SMALL_BIG_EXT, 5, 0, 1, 2],
        // `COMPRESSED` wrappers with lying size fields / non-zlib payloads.
        vec![VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 1, 0],
        vec![VERSION_MAGIC, COMPRESSED_EXT, 255, 255, 255, 255, 1, 2, 3, 4],
        vec![VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 0],
    ]);

    for image in &corpus {
        let mut h = heap(4096);
        let mut a = AtomTable::new();
        // Must return (Ok or Err) — never panic and never read out of bounds.
        let _ = decode_term(image, &mut h, &mut a);
    }
}
