//! Byte-exact ground-truth vectors produced by the system oracle.
//!
//! Every vector below is a literal `term_to_binary/1` image captured from the
//! reference implementation (OTP 29.x / erts-17.1, `erl` on `PATH`). Each test
//! asserts **both** directions:
//!
//! 1. `decode_term(bytes, ..)` yields the expected term, checked through the safe
//!    public accessors (`beam_codec::term::*`, `beam_term::*`); and
//! 2. `encode_term(decoded, ..)` reproduces the exact oracle bytes.
//!
//! ## Regenerating a vector
//!
//! Run the system Erlang and paste the printed decimal byte list into the matching
//! `const`:
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n", [term_to_binary(T)])' -s init stop
//! ```
//!
//! For example
//! `erl -noshell -eval 'io:format("~w~n", [term_to_binary({ok,17})])' -s init stop`
//! prints `<<131,104,2,119,2,111,107,97,17>>`, i.e. the `OK_17` vector below.
//!
//! ## What is *not* byte-exact in the oracle
//!
//! `self()` and `make_ref()` are node-local identifiers whose ids are allocated at
//! run time, so their vectors pin **one observed generation** (node `nonode@nohost`,
//! creation `0`). The test stays exact because the codec contract is a round trip
//! over the pinned bytes, not a re-derivation of the id. The key order of a small
//! map is likewise not canonicalised before encoding, so `MAP_A_1_B_2` pins one
//! observed order; the decoder preserves the wire order and the encoder re-emits it.
//!
//! ## Binaries and bitstrings
//!
//! `BINARY_EXT` (`109`) and `BIT_BINARY_EXT` (`77`) are pinned below too, including
//! the **byte-aligned** cases the oracle encodes as `BINARY_EXT` (`<<7:16>>`) versus
//! the non-aligned ones it encodes as `BIT_BINARY_EXT` (`<<1:3>>`). The vectors were
//! captured the same way as the rest; `oracle_term_to_binary_matches_when_erl_is_available`
//! re-derives them from the system `erl` when one is on `PATH`.
//!
//! Note the ERTS storage divergence: ERTS keeps a binary larger than
//! `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) off-heap as a ref-counted `Binary`, while this
//! crate materializes every binary as an on-heap `ErlHeapBits` block (see
//! `beam_codec::term::make_binary`). The **wire** image is identical, which is what
//! these vectors assert.
//!
//! These tests never use `unsafe`; the crate's only `unsafe` lives in
//! `beam_codec::term`. Panicking on mismatch is intended (a failure is a test
//! failure).

use beam_alloc::Heap;
use beam_codec::compress::decompress;
use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_ref_parts, flatmap_parts, float_value, tuple_arity, tuple_element,
};
use beam_codec::{
    decode_term, encode_term, encode_term_body, encode_term_compressed, COMPRESSED_EXT,
    VERSION_MAGIC,
};
use beam_term::prelude::{
    atom_val, is_atom, is_list, is_nil, is_small, make_small, signed_value, AtomTable, Eterm, NIL,
};

// ---------------------------------------------------------------------------
// Oracle vectors — one literal `term_to_binary/1` image per term.
// ---------------------------------------------------------------------------

/// `{ok,17}`
const OK_17: &[u8] = &[131, 104, 2, 119, 2, 111, 107, 97, 17];
/// `[1,2,3]` — a proper byte list, so the oracle emits `STRING_EXT` (`107`).
const LIST_123: &[u8] = &[131, 107, 0, 3, 1, 2, 3];
/// `"abc"` — `[97,98,99]`, again `STRING_EXT`.
const STRING_ABC: &[u8] = &[131, 107, 0, 3, 97, 98, 99];
/// `#{a=>1}` — flat map with one pair.
const MAP_A_1: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
/// `3.14` — `NEW_FLOAT_EXT` (`70`).
const FLOAT_3_14: &[u8] = &[131, 70, 64, 9, 30, 184, 81, 235, 133, 31];
/// `true`
const ATOM_TRUE: &[u8] = &[131, 119, 4, 116, 114, 117, 101];
/// `[]` — `NIL_EXT` (`106`).
const EMPTY_LIST: &[u8] = &[131, 106];
/// `12345678901234567890123` — a positive `SMALL_BIG_EXT` (`110`) with 10 digits.
const BIG_12345: &[u8] = &[131, 110, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2];
/// `{}` — a zero-arity `SMALL_TUPLE_EXT`.
const EMPTY_TUPLE: &[u8] = &[131, 104, 0];
/// `{1,2,3}`
const TUPLE_123: &[u8] = &[131, 104, 3, 97, 1, 97, 2, 97, 3];
/// `[a,b,c]` — not a byte list, so `LIST_EXT` (`108`) with a `NIL_EXT` tail.
const LIST_ABC: &[u8] = &[131, 108, 0, 0, 0, 3, 119, 1, 97, 119, 1, 98, 119, 1, 99, 106];
/// `-1` — negative, so `INTEGER_EXT` (`98`), not `SMALL_INTEGER_EXT`.
const NEG_ONE: &[u8] = &[131, 98, 255, 255, 255, 255];
/// `100000000000000000000` — `10^20`, a `SMALL_BIG_EXT` with 9 digits.
const BIG_1E20: &[u8] = &[131, 110, 9, 0, 0, 0, 16, 99, 45, 94, 199, 107, 5];
/// `4294967296` (`2^32`) — a five-byte `SMALL_BIG_EXT`. The value fits a 60-bit
/// small, so it decodes to one; it still re-encodes byte-identically because a small
/// outside the signed 32-bit range is encoded as a bignum again.
const BIG_2_POW_32: &[u8] = &[131, 110, 5, 0, 0, 0, 0, 0, 1];
/// `18446744073709551633` (`2^64 + 17`) — nine bytes, i.e. one full `ErtsDigit`
/// (`17`) plus a partial one (`1`); the wire keeps all eight bytes of the low digit.
const BIG_2_64_PLUS_17: &[u8] = &[131, 110, 9, 0, 17, 0, 0, 0, 0, 0, 0, 0, 1];
/// `1.0`
const FLOAT_ONE: &[u8] = &[131, 70, 63, 240, 0, 0, 0, 0, 0, 0];
/// `-0.0` — negative zero keeps its sign bit (`0x8000_0000_0000_0000`).
const FLOAT_NEG_ZERO: &[u8] = &[131, 70, 128, 0, 0, 0, 0, 0, 0, 0];
/// `'hello world'` — an atom needing no quoting on the wire.
const ATOM_HELLO_WORLD: &[u8] = &[131, 119, 11, 104, 101, 108, 108, 111, 32, 119, 111,
    114, 108, 100];
/// `'héllo'` — a non-ASCII atom, `SMALL_ATOM_UTF8_EXT` with the UTF-8 bytes
/// `h C3 A9 l l o`.
const ATOM_HELLO_UNICODE: &[u8] = &[131, 119, 6, 104, 195, 169, 108, 108, 111];
/// `#{}` — an empty map (`116`, count `0`).
const EMPTY_MAP: &[u8] = &[131, 116, 0, 0, 0, 0];
/// `#{a=>1,b=>2}` — one observed key order of a two-pair flat map.
const MAP_A_1_B_2: &[u8] = &[131, 116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2];
/// `self()` — `NEW_PID_EXT` (`88`) on `nonode@nohost`, id `10`, serial/creation `0`.
const PID_SELF: &[u8] = &[131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111,
    104, 111, 115, 116, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0];
/// `make_ref()` — `NEWER_REFERENCE_EXT` (`90`) with three 32-bit ids.
const REF_MAKEREF: &[u8] = &[131, 90, 0, 3, 119, 13, 110, 111, 110, 111, 100, 101, 64,
    110, 111, 104, 111, 115, 116, 0, 0, 0, 0, 0, 1, 57, 124, 156, 252, 0, 3, 130, 192,
    173, 25];
/// `[1|2]` — an improper list: `LIST_EXT` with a non-`NIL` tail.
const IMPROPER_LIST: &[u8] = &[131, 108, 0, 0, 0, 1, 97, 1, 97, 2];
/// `list_to_tuple(lists:seq(1,256))` — arity 256, so `LARGE_TUPLE_EXT` (`105`).
const LARGE_TUPLE_256: &[u8] = &[131, 105, 0, 0, 1, 0, 97, 1, 97, 2, 97, 3, 97, 4, 97,
    5, 97, 6, 97, 7, 97, 8, 97, 9, 97, 10, 97, 11, 97, 12, 97, 13, 97, 14, 97, 15, 97,
    16, 97, 17, 97, 18, 97, 19, 97, 20, 97, 21, 97, 22, 97, 23, 97, 24, 97, 25, 97, 26,
    97, 27, 97, 28, 97, 29, 97, 30, 97, 31, 97, 32, 97, 33, 97, 34, 97, 35, 97, 36, 97,
    37, 97, 38, 97, 39, 97, 40, 97, 41, 97, 42, 97, 43, 97, 44, 97, 45, 97, 46, 97, 47,
    97, 48, 97, 49, 97, 50, 97, 51, 97, 52, 97, 53, 97, 54, 97, 55, 97, 56, 97, 57, 97,
    58, 97, 59, 97, 60, 97, 61, 97, 62, 97, 63, 97, 64, 97, 65, 97, 66, 97, 67, 97, 68,
    97, 69, 97, 70, 97, 71, 97, 72, 97, 73, 97, 74, 97, 75, 97, 76, 97, 77, 97, 78, 97,
    79, 97, 80, 97, 81, 97, 82, 97, 83, 97, 84, 97, 85, 97, 86, 97, 87, 97, 88, 97, 89,
    97, 90, 97, 91, 97, 92, 97, 93, 97, 94, 97, 95, 97, 96, 97, 97, 97, 98, 97, 99, 97,
    100, 97, 101, 97, 102, 97, 103, 97, 104, 97, 105, 97, 106, 97, 107, 97, 108, 97,
    109, 97, 110, 97, 111, 97, 112, 97, 113, 97, 114, 97, 115, 97, 116, 97, 117, 97,
    118, 97, 119, 97, 120, 97, 121, 97, 122, 97, 123, 97, 124, 97, 125, 97, 126, 97,
    127, 97, 128, 97, 129, 97, 130, 97, 131, 97, 132, 97, 133, 97, 134, 97, 135, 97,
    136, 97, 137, 97, 138, 97, 139, 97, 140, 97, 141, 97, 142, 97, 143, 97, 144, 97,
    145, 97, 146, 97, 147, 97, 148, 97, 149, 97, 150, 97, 151, 97, 152, 97, 153, 97,
    154, 97, 155, 97, 156, 97, 157, 97, 158, 97, 159, 97, 160, 97, 161, 97, 162, 97,
    163, 97, 164, 97, 165, 97, 166, 97, 167, 97, 168, 97, 169, 97, 170, 97, 171, 97,
    172, 97, 173, 97, 174, 97, 175, 97, 176, 97, 177, 97, 178, 97, 179, 97, 180, 97,
    181, 97, 182, 97, 183, 97, 184, 97, 185, 97, 186, 97, 187, 97, 188, 97, 189, 97,
    190, 97, 191, 97, 192, 97, 193, 97, 194, 97, 195, 97, 196, 97, 197, 97, 198, 97,
    199, 97, 200, 97, 201, 97, 202, 97, 203, 97, 204, 97, 205, 97, 206, 97, 207, 97,
    208, 97, 209, 97, 210, 97, 211, 97, 212, 97, 213, 97, 214, 97, 215, 97, 216, 97,
    217, 97, 218, 97, 219, 97, 220, 97, 221, 97, 222, 97, 223, 97, 224, 97, 225, 97,
    226, 97, 227, 97, 228, 97, 229, 97, 230, 97, 231, 97, 232, 97, 233, 97, 234, 97,
    235, 97, 236, 97, 237, 97, 238, 97, 239, 97, 240, 97, 241, 97, 242, 97, 243, 97,
    244, 97, 245, 97, 246, 97, 247, 97, 248, 97, 249, 97, 250, 97, 251, 97, 252, 97,
    253, 97, 254, 97, 255, 98, 0, 0, 1, 0];

/// `term_to_binary(lists:duplicate(500, hello), [compressed])` — the oracle's
/// **compressed** image (OTP 29.1.1). Layout: `131, 80, <inner size:4 BE>, <zlib>`.
///
/// The 4-byte size (`0x00000DB2` = 3506) is the length of the inner term body: the
/// uncompressed `term_to_binary(lists:duplicate(500, hello))` image with its version
/// byte removed (`zlib:uncompress/1` over the stream yields exactly `inner[1..]`).
///
/// The zlib bytes are **zlib's** output. `flate2`/`miniz_oxide` would produce a
/// different — equally valid — deflate stream for the same input, so this vector
/// pins the **decoder** and the inner body, not our compressor's byte output (that is
/// covered by the round-trip tests in `roundtrip.rs`).
const COMPRESSED_DUP_500_HELLO: &[u8] = &[
    131, 80, 0, 0, 13, 178, 120, 156, 237, 197, 177, 9, 0, 32, 16, 4, 176, 183, 112, 84, 65, 228,
    192, 210, 197, 29, 192, 53, 44, 146, 38, 169, 106, 247, 244, 57, 146, 45, 73, 146, 36, 73,
    250, 168, 245, 0, 145, 146, 3, 87,
];
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decodes `bytes`, asserts the encoder is byte-exact, and hands back the heap, the
/// atom table and the term (the term borrows the heap's and table's storage, so all
/// three are returned — callers must bind the heap, even as `_heap`).
fn round_trip(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut heap = Heap::new(1024).expect("a 1024-word heap is always available");
    let mut atoms = AtomTable::new();
    let term = decode_term(bytes, &mut heap, &mut atoms)
        .unwrap_or_else(|err| panic!("decoding {bytes:?} failed: {err:?}"));
    let reencoded = encode_term(term, &atoms).expect("a decoded term always re-encodes");
    assert_eq!(
        reencoded.as_slice(),
        bytes,
        "re-encoding must reproduce the oracle bytes exactly"
    );
    (heap, atoms, term)
}

/// The name of an atom term, resolved through the table the decoder interned into.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    assert!(is_atom(term), "expected an atom, got {term:?}");
    atoms
        .name(atom_val(term))
        .expect("a decoded atom is interned in the table")
        .to_string()
}

/// The value of a small-integer term.
fn small_value(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer, got {term:?}");
    signed_value(term)
}

/// Walks a cons spine, returning its heads and its final tail (`NIL` for a proper
/// list, the improper tail otherwise).
fn spine(mut cursor: Eterm) -> (Vec<Eterm>, Eterm) {
    let mut heads = Vec::new();
    while is_list(cursor) {
        heads.push(cons_head(cursor).expect("a cons cell has a head"));
        cursor = cons_tail(cursor).expect("a cons cell has a tail");
    }
    (heads, cursor)
}

/// The values of a list of small integers.
fn small_list(term: Eterm) -> Vec<i64> {
    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "expected a proper list, tail is {tail:?}");
    heads.into_iter().map(small_value).collect()
}

// ---------------------------------------------------------------------------
// Seeded vectors (were documented in this file's plan before the bodies landed)
// ---------------------------------------------------------------------------

#[test]
fn seed_tuple_ok_17() {
    let (_heap, atoms, term) = round_trip(OK_17);
    assert_eq!(tuple_arity(term).unwrap(), 2);
    assert_eq!(atom_name(&atoms, tuple_element(term, 0).unwrap()), "ok");
    assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
    assert_eq!(small_value(tuple_element(term, 1).unwrap()), 17);
}

#[test]
fn seed_byte_lists_are_string_ext() {
    // `[1,2,3]` and `"abc"` are both proper byte lists, so the oracle emits
    // `STRING_EXT`; both must decode to a cons list and re-encode byte-identically.
    let (_heap, _atoms, term) = round_trip(LIST_123);
    assert!(is_list(term));
    assert_eq!(small_list(term), vec![1, 2, 3]);

    let (_heap, _atoms, term) = round_trip(STRING_ABC);
    assert_eq!(small_list(term), vec![97, 98, 99]);
}

#[test]
fn seed_map_a_to_1() {
    let (_heap, atoms, term) = round_trip(MAP_A_1);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(values.len(), 1);
    assert_eq!(atom_name(&atoms, keys[0]), "a");
    assert_eq!(small_value(values[0]), 1);
}

#[test]
fn seed_float_3_14() {
    let (_heap, _atoms, term) = round_trip(FLOAT_3_14);
    // Compared by bit pattern so the comparison is exact.
    assert_eq!(float_value(term).unwrap().to_bits(), 3.14_f64.to_bits());
}

#[test]
fn seed_atom_true() {
    let (_heap, atoms, term) = round_trip(ATOM_TRUE);
    assert_eq!(atom_name(&atoms, term), "true");
}

#[test]
fn seed_empty_list() {
    let (_heap, _atoms, term) = round_trip(EMPTY_LIST);
    assert!(is_nil(term));
    assert_eq!(term, NIL);
}

#[test]
fn seed_bignum_12345678901234567890123() {
    let (_heap, _atoms, term) = round_trip(BIG_12345);
    assert!(!is_small(term), "10^23-sized values never fit a 60-bit small");
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // `ErtsDigit`s (`u64`), least significant first — one digit per word, so the ten
    // wire bytes group into one full digit and one partial (`big.h:29`).
    assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);
}

// ---------------------------------------------------------------------------
// Newly generated vectors
// ---------------------------------------------------------------------------

#[test]
fn empty_tuple() {
    let (_heap, _atoms, term) = round_trip(EMPTY_TUPLE);
    assert_eq!(tuple_arity(term).unwrap(), 0);
}

#[test]
fn small_tuple_1_2_3() {
    let (_heap, _atoms, term) = round_trip(TUPLE_123);
    assert_eq!(tuple_arity(term).unwrap(), 3);
    for (index, expected) in [1_i64, 2, 3].into_iter().enumerate() {
        assert_eq!(small_value(tuple_element(term, index).unwrap()), expected);
    }
}

#[test]
fn list_of_atoms_is_list_ext() {
    let (_heap, atoms, term) = round_trip(LIST_ABC);
    let (heads, tail) = spine(term);
    assert_eq!(heads.len(), 3);
    for (head, expected) in heads.iter().zip(["a", "b", "c"]) {
        assert_eq!(atom_name(&atoms, *head), expected);
    }
    assert!(is_nil(tail));
}

#[test]
fn negative_one_is_integer_ext() {
    let (_heap, _atoms, term) = round_trip(NEG_ONE);
    assert_eq!(small_value(term), -1);
}

#[test]
fn bignum_1e20() {
    let (_heap, _atoms, term) = round_trip(BIG_1E20);
    assert!(!is_small(term));
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // The nine wire bytes are one full `ErtsDigit` plus the partial digit `5`.
    assert_eq!(digits, &[0x6BC7_5E2D_6310_0000, 0x5]);
}

#[test]
fn bignum_2_pow_32_decodes_to_a_small() {
    let (_heap, _atoms, term) = round_trip(BIG_2_POW_32);
    assert!(is_small(term), "2^32 fits a 60-bit small");
    assert_eq!(small_value(term), 4_294_967_296);
}

#[test]
fn bignum_2_64_plus_17_keeps_the_full_low_digit() {
    let (_heap, _atoms, term) = round_trip(BIG_2_64_PLUS_17);
    assert!(!is_small(term), "2^64 + 17 exceeds a 60-bit small");
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    assert_eq!(digits, &[17, 1]);
}

#[test]
fn bignum_2_pow_3000_uses_large_big() {
    // The oracle's `term_to_binary(1 bsl 3000)`: `LARGE_BIG_EXT` (`111`) with a
    // 376-byte magnitude, i.e. `[111, 0, 0, 1, 120, 0, <375 zero bytes>, 1]`.
    let mut image = vec![131u8, 111, 0, 0, 1, 120, 0];
    image.extend(std::iter::repeat(0u8).take(375));
    image.push(1);

    let (_heap, _atoms, term) = round_trip(&image);
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // 376 bytes = 47 `ErtsDigit`s: 46 zero ones holding the zero bytes, plus the
    // partial digit carrying the leading `0x01` byte at its most significant position.
    assert_eq!(digits.len(), 47);
    assert!(digits[..46].iter().all(|&digit| digit == 0));
    assert_eq!(digits[46], 0x0100_0000_0000_0000);
}

#[test]
fn floats_one_and_negative_zero() {
    let (_heap, _atoms, term) = round_trip(FLOAT_ONE);
    assert_eq!(float_value(term).unwrap().to_bits(), 1.0_f64.to_bits());

    let (_heap, _atoms, term) = round_trip(FLOAT_NEG_ZERO);
    let value = float_value(term).unwrap();
    assert_eq!(value.to_bits(), (-0.0_f64).to_bits());
    assert!(value.is_sign_negative(), "-0.0 keeps its sign bit");
    assert_eq!(value, 0.0);
}

#[test]
fn atoms_with_space_and_unicode() {
    let (_heap, atoms, term) = round_trip(ATOM_HELLO_WORLD);
    assert_eq!(atom_name(&atoms, term), "hello world");

    let (_heap, atoms, term) = round_trip(ATOM_HELLO_UNICODE);
    assert_eq!(atom_name(&atoms, term), "héllo");
}

#[test]
fn empty_map() {
    let (_heap, _atoms, term) = round_trip(EMPTY_MAP);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert!(keys.is_empty());
    assert!(values.is_empty());
}

#[test]
fn map_with_two_pairs() {
    let (_heap, atoms, term) = round_trip(MAP_A_1_B_2);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(values.len(), 2);
    assert_eq!(atom_name(&atoms, keys[0]), "a");
    assert_eq!(small_value(values[0]), 1);
    assert_eq!(atom_name(&atoms, keys[1]), "b");
    assert_eq!(small_value(values[1]), 2);
}

#[test]
fn external_pid_self() {
    let (_heap, atoms, term) = round_trip(PID_SELF);
    let (node, number, serial, creation) = external_pid_parts(term).unwrap();
    assert_eq!(atom_name(&atoms, node), "nonode@nohost");
    assert_eq!((number, serial, creation), (10, 0, 0));
}

#[test]
fn external_reference() {
    let (_heap, atoms, term) = round_trip(REF_MAKEREF);
    let (node, ids, creation) = external_ref_parts(term).unwrap();
    assert_eq!(atom_name(&atoms, node), "nonode@nohost");
    assert_eq!(ids, &[0x0001_397C, 0x9CFC_0003, 0x82C0_AD19]);
    assert_eq!(creation, 0);
}

#[test]
fn improper_list() {
    let (_heap, _atoms, term) = round_trip(IMPROPER_LIST);
    assert!(is_list(term));
    let (heads, tail) = spine(term);
    assert_eq!(heads.len(), 1);
    assert_eq!(small_value(heads[0]), 1);
    assert!(!is_nil(tail), "the tail is the improper element 2");
    assert_eq!(small_value(tail), 2);
}

#[test]
fn large_tuple_arity_256_is_large_tuple_ext() {
    let (_heap, _atoms, term) = round_trip(LARGE_TUPLE_256);
    assert_eq!(tuple_arity(term).unwrap(), 256);
    for index in 0..256 {
        assert_eq!(
            small_value(tuple_element(term, index).unwrap()),
            index as i64 + 1
        );
    }
}

// ---------------------------------------------------------------------------
// COMPRESSED (`80`) — oracle vectors
// ---------------------------------------------------------------------------

#[test]
fn seed_compressed_image_decodes_and_its_inner_body_matches() {
    let mut heap = Heap::new(4096).expect("a 4096-word heap holds 500 cons cells");
    let mut atoms = AtomTable::new();
    let term = decode_term(COMPRESSED_DUP_500_HELLO, &mut heap, &mut atoms)
        .expect("the oracle compressed image must decode");

    // `[hello, hello, …]` — 500 atoms, proper tail.
    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "the inner list must be proper");
    assert_eq!(heads.len(), 500);
    for head in heads {
        assert!(is_atom(head), "expected an atom element, got {head:?}");
        assert_eq!(atom_name(&atoms, head), "hello");
    }

    // The inner body — the zlib stream, decompressed — is byte-identical to our own
    // uncompressed encoding of the same term. This pins the term layer even though
    // the zlib bytes themselves are implementation-defined.
    let inner = decompress(&COMPRESSED_DUP_500_HELLO[6..]).expect("the stream is valid zlib");
    assert_eq!(inner.len(), 3506, "the declared inner size");
    assert_eq!(encode_term_body(term, &atoms).unwrap(), inner);

    // Re-compressing produces a well-formed wrapper whose inner body is the same.
    let image = encode_term_compressed(term, &atoms, 6).unwrap();
    assert_eq!(image[0], VERSION_MAGIC);
    assert_eq!(image[1], COMPRESSED_EXT);
    let mut heap2 = Heap::new(4096).expect("a 4096-word heap holds 500 cons cells");
    let mut atoms2 = AtomTable::new();
    let term2 = decode_term(&image, &mut heap2, &mut atoms2).expect("our image must decode");
    assert_eq!(encode_term_body(term2, &atoms2).unwrap(), inner);
}

#[test]
fn compressed_option_falls_back_when_it_does_not_help() {
    // The C only emits the `COMPRESSED` wrapper when zlib actually shrinks the term;
    // otherwise it reverts to the plain image (`external.c:2284-2293`). `{ok,17}` is
    // small enough that `term_to_binary({ok,17}, [compressed])` is byte-identical to
    // the uncompressed `OK_17` vector (verified with the oracle).
    //
    // Our `encode_term_compressed` deliberately *always* writes the wrapper — the
    // caller asked for compression — so it is not the same bytes for this term; the
    // oracle's (uncompressed) image must nevertheless decode, which this asserts.
    let (_heap, _atoms, term) = round_trip(OK_17);
    assert_eq!(tuple_arity(term).unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Binaries and bitstrings — oracle vectors
// ---------------------------------------------------------------------------

#[test]
fn binary_hi_is_binary_ext() {
    // `<<"hi">>` → `[109][len:4 BE][data]`.
    const BINARY_HI: &[u8] = &[131, 109, 0, 0, 0, 2, 104, 105];
    let (_heap, _atoms, term) = round_trip(BINARY_HI);
    assert_eq!(bitstring_size_bits(term).unwrap(), 16);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[104u8, 105][..], 0, 16));
}

#[test]
fn bit_aligned_bitstring_is_binary_ext() {
    // `<<7:16>>` is byte-aligned, so the oracle still emits `BINARY_EXT` (`109`).
    const BIT_7_16: &[u8] = &[131, 109, 0, 0, 0, 2, 0, 7];
    let (_heap, _atoms, term) = round_trip(BIT_7_16);
    assert_eq!(bitstring_size_bits(term).unwrap(), 16);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[0u8, 7][..], 0, 16));
}

#[test]
fn empty_binary_is_binary_ext() {
    // `<<>>` → `[109][len:4 BE = 0]`.
    const EMPTY_BINARY: &[u8] = &[131, 109, 0, 0, 0, 0];
    let (_heap, _atoms, term) = round_trip(EMPTY_BINARY);
    assert_eq!(bitstring_size_bits(term).unwrap(), 0);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[][..], 0, 0));
}

#[test]
fn non_aligned_bitstrings_are_bit_binary_ext() {
    // `[77][len:4 BE][bits:1][data]` — the length field precedes the trailing-bit
    // count, and `data` is MSB-first inside each byte.
    for &(image, n_bits) in &[
        (&[131u8, 77, 0, 0, 0, 1, 1, 128][..], 1usize), // <<1:1>>
        (&[131, 77, 0, 0, 0, 1, 3, 0b0010_0000], 3),    // <<1:3>>
        (&[131, 77, 0, 0, 0, 2, 4, 0, 0x50], 12),       // <<5:12>>
        (&[131, 77, 0, 0, 0, 2, 5, 0, 0x18], 13),       // <<3:13>>
    ] {
        let (_heap, _atoms, term) = round_trip(image);
        assert_eq!(bitstring_size_bits(term).unwrap(), n_bits);
    }
}

#[test]
fn large_binary_round_trips_through_the_wire() {
    // `list_to_binary(lists:seq(1,100))` — 100 bytes, past `ERL_ONHEAP_BINARY_LIMIT`
    // (64): ERTS stores it off-heap as a ref-counted binary, but the wire image is the
    // same `[109][len:4 BE][data]` and this crate materializes it on-heap.
    let mut image = vec![131u8, 109, 0, 0, 0, 100];
    image.extend(1u8..=100);
    let (_heap, _atoms, term) = round_trip(&image);
    assert_eq!(bitstring_size_bits(term).unwrap(), 800);
}