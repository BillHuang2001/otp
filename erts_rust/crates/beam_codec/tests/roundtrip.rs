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
//! and byte lists up to the `STRING_EXT`/`LIST_EXT` switchover, small maps and
//! pids/ports/references.
//!
//! Two documented *non*-byte-identical transformations are asserted explicitly: a
//! bignum whose magnitude fits the 60-bit small range decodes to a small (matching the
//! C `dec_term`), and a legacy `FLOAT_EXT` re-encodes as `NEW_FLOAT_EXT`.
//!
//! Negative cases cover truncation at every byte offset, unknown/unsupported tags,
//! over-long atoms and bignums (`BeamError::SystemLimit`), oversized maps
//! (`error::unsupported`), a malformed bignum sign and trailing bytes. Every malformed
//! input must yield an error — never a panic, never a silently truncated or
//! mis-encoded term.

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::{
    atom_val, is_atom, is_list, is_nil, is_small, make_atom, make_small, signed_value, AtomTable,
    Eterm, MAX_SMALL, MIN_SMALL, NIL,
};

use beam_codec::compress::{compress, decompress};
use beam_codec::error::is_unsupported;
use beam_codec::term::{
    big_parts, bitstring_bytes, cons_head, cons_tail, external_pid_parts, external_port_parts, external_ref_parts,
    flatmap_parts, float_value, make_big, make_external_pid, make_external_port, make_external_ref,
    make_flatmap, make_float, tuple_arity, tuple_element,
};
use beam_codec::{
    decode_term, encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    AtomEncoding, Decoder, Encoder, ATOM_EXT, ATOM_UTF8_EXT, BINARY_EXT, BIT_BINARY_EXT, COMPRESSED_EXT, FLOAT_EXT,
    INTEGER_EXT, LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, MAP_EXT, NEWER_REFERENCE_EXT,
    NEW_FLOAT_EXT, NEW_PID_EXT, NEW_PORT_EXT, NEW_REFERENCE_EXT, NIL_EXT, SMALL_ATOM_UTF8_EXT,
    SMALL_BIG_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT, VERSION_MAGIC,
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
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
        f64::NAN.to_bits(),
        0x7ff0_0000_0000_0001, // signaling NaN
        0x7ff8_0000_dead_beef, // quiet NaN with a payload
        0xfff0_0000_0000_0001, // negative signaling NaN
        1,                     // the smallest subnormal
        f64::MIN_POSITIVE.to_bits(),
        f64::MIN.to_bits(),
        f64::MAX.to_bits(),
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
fn external_ports_round_trip_and_reject_wide_ids() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    for &(number, creation) in &[(0u64, 0u32), (0x1234, 5), (u64::from(u32::MAX), u32::MAX)] {
        let mut h1 = heap(64);
        let p = make_external_port(&mut h1, node, number, creation).unwrap();

        let (b1, t2, _h2, a2) = roundtrip(p, &atoms, 64);

        assert_eq!(b1[1], NEW_PORT_EXT);
        let (n2, num2, cre2) = external_port_parts(t2).unwrap();
        assert_eq!(a2.name(atom_val(n2)).unwrap(), "nonode@nohost");
        assert_eq!((num2, cre2), (number, creation));
    }

    // A 64-bit port id does not fit `NEW_PORT_EXT`'s 32-bit field: `SystemLimit`, never
    // a silent truncation.
    let mut h1 = heap(64);
    let wide = make_external_port(&mut h1, node, 1u64 << 32, 0).unwrap();
    assert!(matches!(
        encode_term(wide, &atoms),
        Err(BeamError::SystemLimit)
    ));
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
fn compressed_image_round_trips_a_small_term() {
    let mut atoms = AtomTable::new();
    let ok = make_atom(atoms.intern("ok"));
    let mut h = heap(64);
    let term = h.tuple(&[ok, make_small(17)]).unwrap();

    let image = encode_term_compressed(term, &atoms, 6).unwrap();
    let body = encode_term_body(term, &atoms).unwrap();
    assert_eq!(declared_inner_size(&image), body.len());
    // The zlib stream carries exactly the term body (no version byte).
    assert_eq!(decompress(&image[6..]).unwrap(), body);

    let mut h2 = heap(64);
    let mut a2 = AtomTable::new();
    let term2 = decode_term(&image, &mut h2, &mut a2).unwrap();
    assert_term_eq(term, &atoms, term2, &a2);
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
fn compressed_wrapper_bytes_are_well_formed_on_every_level() {
    // A term where compression pays off at every level (0 stores, 9 best).
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let term = list(&mut h, &[hello; 256], NIL);
    let body = encode_term_body(term, &atoms).unwrap();

    for level in 0..=9u8 {
        let image = encode_term_compressed(term, &atoms, level).unwrap();
        assert_eq!(declared_inner_size(&image), body.len(), "level {level}");
        assert_eq!(decompress(&image[6..]).unwrap(), body, "level {level}");

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

fn assert_unsupported(bytes: &[u8]) {
    let err = decode_err(bytes);
    assert!(is_unsupported(&err), "expected unsupported, got {err:?}");
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
fn over_long_atoms_are_system_limit() {
    // A crafted `ATOM_UTF8_EXT` carrying 256 characters.
    let mut image = vec![VERSION_MAGIC, ATOM_UTF8_EXT, 0x01, 0x00];
    image.extend(std::iter::repeat(b'a').take(256));
    assert_system_limit(&image);

    // Interning a 256-character name and encoding it is a `SystemLimit` too.
    let mut atoms = AtomTable::new();
    let long = make_atom(atoms.intern(&"a".repeat(256)));
    assert!(matches!(
        encode_term(long, &atoms),
        Err(BeamError::SystemLimit)
    ));
}

#[test]
fn oversized_bignums_are_system_limit() {
    // `LARGE_BIG_EXT` with a byte count past `BIG_ARITY_MAX * sizeof(ErtsDigit)`
    // (65535 * 8 = 524280 bytes, `external.c:6146`).
    assert_system_limit(&[VERSION_MAGIC, LARGE_BIG_EXT, 0x00, 0x08, 0x00, 0x19]);

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
fn signed_bignum_with_bad_sign_byte_is_bad_arg() {
    // The sign byte is neither 0 nor 1.
    assert_bad_arg(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 2, 5]);
}

#[test]
fn maps_with_more_than_32_pairs_are_unsupported() {
    // 33 pairs needs the HAMT builder the M1 term layer does not have.
    assert_unsupported(&[VERSION_MAGIC, MAP_EXT, 0, 0, 0, 33]);
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
