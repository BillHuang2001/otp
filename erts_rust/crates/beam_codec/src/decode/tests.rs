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

    //! Decode tests: the seeded `term_to_binary` vectors, term structure asserted
    //! through the safe accessors, plus the error mapping (truncation at every
    //! offset, bad tags, version rules, wire bounds and unsupported tags).
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure); the
    //! production bodies above never do.

    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::prelude::{
        atom_val, is_internal_pid, is_internal_port, is_list, is_nil, is_small, pid_number,
        pid_serial, port_number, signed_value,
    };

    use crate::term::{
        big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
        external_port_parts, external_ref_parts, flatmap_parts, float_value, tuple_arity,
        tuple_element,
    };

    /// `{ok,17}` (oracle: `term_to_binary({ok,17})`).
    const OK_17: [u8; 9] = [131, 104, 2, 119, 2, 111, 107, 97, 17];
    /// `[1,2,3]` as `STRING_EXT` (oracle: `term_to_binary([1,2,3])`).
    const LIST_123: [u8; 7] = [131, 107, 0, 3, 1, 2, 3];
    /// `#{a=>1}` (oracle: `term_to_binary(#{a=>1})`).
    const MAP_A_1: [u8; 11] = [131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

    /// Decodes a full image, returning the heap and atom table the term points
    /// into (the term borrows their storage, so all three are returned).
    pub(crate) fn decode(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
        let mut heap = Heap::new(256).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let term = decode_term(bytes, &mut heap, &mut atoms).expect("valid image decodes");
        (heap, atoms, term)
    }

    /// The error `decode_term` produces for `bytes`.
    fn decode_err(bytes: &[u8]) -> BeamError {
        let mut heap = Heap::new(64).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        decode_term(bytes, &mut heap, &mut atoms).expect_err("input must be rejected")
    }

    pub(crate) fn assert_bad_arg(bytes: &[u8]) {
        match decode_err(bytes) {
            BeamError::BadArg => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    /// The UTF-8 `"n1"` node atom, as it appears in an external identifier.
    const NODE_N1: [u8; 4] = [119, 2, 110, 49];

    #[test]
    fn integers_small_and_32_bit() {
        let (_h, _a, term) = decode(&[131, 97, 255]);
        assert_eq!(term, make_small(255));

        let (_h, _a, term) = decode(&[131, 98, 0, 0, 0, 7]);
        assert_eq!(term, make_small(7));

        // `INTEGER_EXT` is a signed 32-bit two's-complement value.
        let (_h, _a, term) = decode(&[131, 98, 255, 255, 255, 255]);
        assert_eq!(term, make_small(-1));
        assert_eq!(signed_value(term), -1);

        let (_h, _a, term) = decode(&[131, 98, 128, 0, 0, 0]);
        assert_eq!(signed_value(term), -2_147_483_648);
    }

    #[test]
    fn floats_new_and_legacy() {
        // `NEW_FLOAT_EXT` — the oracle encoding of `3.14`.
        let (_h, _a, term) = decode(&[131, 70, 64, 9, 30, 184, 81, 235, 133, 31]);
        assert_eq!(float_value(term).unwrap(), 3.14);

        // `FLOAT_EXT` — a 31-byte NUL-padded `%.20e` string.
        let mut image = vec![131, 99];
        image.extend_from_slice(b"3.14000000000000000000e+00");
        image.push(0);
        image.resize(2 + 31, 0);
        let (_h, _a, term) = decode(&image);
        assert_eq!(float_value(term).unwrap(), 3.14);

        // Negative zero survives bit-for-bit.
        let (_h, _a, term) = decode(&[131, 70, 128, 0, 0, 0, 0, 0, 0, 0]);
        assert!(float_value(term).unwrap().is_sign_negative());

        // A non-finite `NEW_FLOAT_EXT` is rejected: the wire cannot carry an
        // infinity or NaN (`if (!erts_isfinite(ff.fd)) goto error;`,
        // `external.c:4667-4669`), matching the oracle's `badarg`.
        assert_bad_arg(&[131, 70, 127, 240, 0, 0, 0, 0, 0, 0]); // +inf
        assert_bad_arg(&[131, 70, 255, 240, 0, 0, 0, 0, 0, 0]); // -inf
        assert_bad_arg(&[131, 70, 127, 248, 0, 0, 0, 0, 0, 0]); // quiet NaN
        assert_bad_arg(&[131, 70, 127, 240, 0, 0, 0, 0, 0, 1]); // signaling NaN

        // `FLOAT_EXT` accepts a short, NUL-terminated rendering.
        let mut image = vec![131, 99];
        image.extend_from_slice(b"1.5");
        image.push(0);
        image.resize(2 + 31, 0);
        let (_h, _a, term) = decode(&image);
        assert_eq!(float_value(term).unwrap(), 1.5);

        // The accepted grammar is exactly `[+-]? DIGIT+ [.,] DIGIT+ ([eE] [+-]?
        // DIGIT+)?` with a finite result — the C `sys_chars_to_double`
        // (`sys_float.c:72-140`), each form verified live against the oracle. A
        // `,` radix is accepted (the C retries `strtod` with the flipped radix
        // character), as are an explicit `+` sign and an uppercase exponent.
        let legacy = |text: &str| -> Vec<u8> {
            let mut image = vec![131, 99];
            image.extend_from_slice(text.as_bytes());
            image.resize(2 + 31, 0);
            image
        };
        for (text, expected) in [
            ("1.0e5", 1.0e5f64),
            ("+1.0", 1.0),
            ("1.0E+05", 1.0e5),
            ("1,5", 1.5),
            ("-0.0", -0.0),
        ] {
            let (_h, _a, term) = decode(&legacy(text));
            assert_eq!(
                float_value(term).unwrap().to_bits(),
                expected.to_bits(),
                "{text:?}"
            );
        }

        // Anything outside that grammar is malformed input: the `inf`/`nan`
        // spellings, a radix-less mantissa, a missing integer or fractional part,
        // leading whitespace, hex floats and trailing junk are all `BadArg` —
        // Rust's `parse::<f64>()` would have accepted several of these.
        for bad_text in [
            "inf", "-inf", "nan", "infinity", "1e5", "1.", ".5", " 1.0", "0x1p3", "1", "+", "", "1.0e",
            "1.5x",
        ] {
            assert_bad_arg(&legacy(bad_text));
        }

        let mut bad = vec![131, 99];
        bad.extend_from_slice(b"not a number");
        bad.resize(2 + 31, 0);
        let err = decode_err(&bad);
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn all_four_atom_tags_decode() {
        // `ATOM_EXT` (Latin-1).
        let (_h, atoms, term) = decode(&[131, 100, 0, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `SMALL_ATOM_EXT` (Latin-1).
        let (_h, atoms, term) = decode(&[131, 115, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `ATOM_UTF8_EXT`.
        let (_h, atoms, term) = decode(&[131, 118, 0, 2, 111, 107]);
        assert_eq!(atoms.name(atom_val(term)), Some("ok"));

        // `SMALL_ATOM_UTF8_EXT`, multi-byte.
        let (_h, atoms, term) = decode(&[131, 119, 2, 0xC3, 0xA9]);
        assert_eq!(atoms.name(atom_val(term)), Some("é"));

        // Latin-1 high bytes map to `U+0080..=U+00FF`.
        let (_h, atoms, term) = decode(&[131, 115, 1, 0xFF]);
        assert_eq!(atoms.name(atom_val(term)), Some("ÿ"));

        // Invalid UTF-8 in a UTF-8 atom tag is malformed input.
        assert_bad_arg(&[131, 118, 0, 1, 0xFF]);
        assert_bad_arg(&[131, 119, 1, 0xFF]);

        // A zero-length atom name is legal.
        let (_h, atoms, term) = decode(&[131, 115, 0]);
        assert_eq!(atoms.name(atom_val(term)), Some(""));
    }

    #[test]
    fn over_long_atom_names_are_bad_arg() {
        // An over-long name is malformed input, not a resource limit: C rejects it
        // in `erts_atom_put`/`erts_atom_get` (`atom.c:258-274`, `atom.c:392-418`)
        // and `dec_term` turns that into `goto error` (`external.c:4529-4539`).
        // 256 Latin-1 characters (one byte each).
        let mut latin1 = vec![131, 100, 1, 0];
        latin1.extend_from_slice(&[b'a'; 256]);
        assert_bad_arg(&latin1);

        // 256 UTF-8 characters.
        let mut utf8 = vec![131, 118, 1, 0];
        utf8.extend_from_slice(&[b'a'; 256]);
        assert_bad_arg(&utf8);

        // Exactly 255 characters is at the limit.
        let mut at_limit = vec![131, 100, 0, 255];
        at_limit.extend_from_slice(&[b'a'; 255]);
        let (_h, atoms, term) = decode(&at_limit);
        assert_eq!(atoms.name(atom_val(term)).map(str::len), Some(255));

        // The limit counts *characters*, not UTF-8 bytes: a 150-character name
        // spanning 300 bytes is legal (`erts_atom_put` bounds the char count,
        // `atom.c:258-274`).
        let mut multi_byte = vec![131, 118, 1, 44];
        for _ in 0..150 {
            multi_byte.extend_from_slice(&[0xC3, 0xA9]); // "é"
        }
        let (_h, atoms, term) = decode(&multi_byte);
        let name = atoms.name(atom_val(term)).unwrap();
        assert_eq!(name.chars().count(), 150);
        assert_eq!(name.len(), 300);
    }

    #[test]
    fn tuples_nil_and_arity_bounds() {
        // `{ok,17}` — arity-2 tuple, first element the atom `ok`.
        let (_h, atoms, term) = decode(&OK_17);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        let first = tuple_element(term, 0).unwrap();
        assert!(is_atom(first));
        assert_eq!(atoms.name(atom_val(first)), Some("ok"));
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
        assert!(tuple_element(term, 2).is_err());

        // The empty tuple is still a tuple.
        let (_h, _a, term) = decode(&[131, 104, 0]);
        assert_eq!(tuple_arity(term).unwrap(), 0);

        // `LARGE_TUPLE_EXT`.
        let (_h, _a, term) = decode(&[131, 105, 0, 0, 0, 2, 97, 1, 97, 2]);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(2));

        // `NIL_EXT`.
        let (_h, _a, term) = decode(&[131, 106]);
        assert_eq!(term, NIL);
        assert!(is_nil(term));

        // An arity above `MAX_ARITYVAL` is a hard wire bound; like every other decode
        // failure the oracle reports `badarg` (never `system_limit`).
        assert_bad_arg(&[131, 105, 1, 0, 0, 0]);
        // `MAX_ARITYVAL` itself is only rejected for lack of elements.
        assert_bad_arg(&[131, 105, 0, 255, 255, 255]);
    }

    #[test]
    fn strings_and_lists() {
        // `STRING_EXT` → a list of smalls (oracle encoding of `[1,2,3]`).
        let (_h, _a, term) = decode(&LIST_123);
        assert!(is_list(term));
        let first = cons_head(term).unwrap();
        let rest = cons_tail(term).unwrap();
        let second = cons_head(rest).unwrap();
        let rest = cons_tail(rest).unwrap();
        let third = cons_head(rest).unwrap();
        assert_eq!(first, make_small(1));
        assert_eq!(second, make_small(2));
        assert_eq!(third, make_small(3));
        assert!(is_nil(cons_tail(rest).unwrap()));

        // The empty string is `NIL` itself.
        let (_h, _a, term) = decode(&[131, 107, 0, 0]);
        assert_eq!(term, NIL);

        // `LIST_EXT`, proper.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 2, 97, 1, 97, 2, 106]);
        assert_eq!(cons_head(term).unwrap(), make_small(1));
        let rest = cons_tail(term).unwrap();
        assert_eq!(cons_head(rest).unwrap(), make_small(2));
        assert!(is_nil(cons_tail(rest).unwrap()));

        // `LIST_EXT`, improper: the tail is the following term.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 1, 97, 7, 97, 9]);
        assert_eq!(cons_head(term).unwrap(), make_small(7));
        assert_eq!(cons_tail(term).unwrap(), make_small(9));

        // An empty `LIST_EXT` is just its tail.
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 0, 97, 5]);
        assert_eq!(term, make_small(5));
        // A non-small list element is fine (list of tuples).
        let (_h, _a, term) = decode(&[131, 108, 0, 0, 0, 1, 104, 0, 106]);
        assert_eq!(tuple_arity(cons_head(term).unwrap()).unwrap(), 0);
    }

    #[test]
    fn maps_flatmap_at_any_pair_count() {
        // `#{a=>1}`.
        let (_h, atoms, term) = decode(&MAP_A_1);
        let (keys, values) = flatmap_parts(term).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(values.len(), 1);
        assert_eq!(atoms.name(atom_val(keys[0])), Some("a"));
        assert_eq!(values[0], make_small(1));

        // An empty map is a flatmap of zero pairs.
        let (_h, _a, term) = decode(&[131, 116, 0, 0, 0, 0]);
        let (keys, values) = flatmap_parts(term).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());

        // 40 pairs (> `MAP_SMALL_MAP_LIMIT`) is a **hash map**: ERTS builds a HAMT for
        // any count above 32 (`external.c:5087`), so the flat path rejects it. A HAMT
        // needs more heap than the shared `decode` helper's tiny arena, so build a
        // bigger one here.
        let mut image = vec![131, 116, 0, 0, 0, 40];
        for i in 0..40u8 {
            image.extend_from_slice(&[97, i]); // SMALL_INTEGER_EXT key
            image.extend_from_slice(&[97, i + 100]); // SMALL_INTEGER_EXT value
        }
        let mut heap = Heap::new(4096).expect("room for a 40-key HAMT");
        let mut atoms = AtomTable::new();
        let term = decode_term(&image, &mut heap, &mut atoms).expect("valid image decodes");
        assert!(
            flatmap_parts(term).is_err(),
            "a > 32-pair map is a HAMT, not a flat map"
        );
        assert_eq!(beam_term::map::map_size(term, &atoms).unwrap(), 40);
        for i in 0..40i64 {
            assert_eq!(
                beam_term::map::map_get(term, make_small(i), &atoms).unwrap(),
                Some(make_small(i + 100))
            );
        }

        // A pair count whose bit 31 is set is malformed: the sizing pass' `(n >> 31) != 0`
        // guard (`external.c:6312`).
        assert_bad_arg(&[131, 116, 0x80, 0x00, 0x00, 0x00]);
        // A pair count that overruns the wire fails on the first truncated pair read —
        // `BadArg`, not the old flatmap `arityval` `SystemLimit` (a HAMT has no
        // `MAX_ARITYVAL` bound).
        assert_bad_arg(&[131, 116, 0x01, 0x00, 0x00, 0x00]);

        // Duplicate keys are rejected. ERTS checks for duplicates in **both**
        // representations (`erts_validate_and_sort_flatmap` for a flat map,
        // `erts_hashmap_from_array(..., 1)` for a HAMT — the `external.c:5438-5456`
        // fixup), so `binary_to_term` is `badarg`; only `maps:from_list/1` collapses.
        // `#{1 => 10, 1 => 20}` — key `1` twice.
        assert_bad_arg(&[131, 116, 0, 0, 0, 2, 97, 1, 97, 10, 97, 1, 97, 20]);
    }

    #[test]
    fn bignums_decode_to_small_or_bignum() {
        // 300 (`0x012C`) fits a small.
        let (_h, _a, term) = decode(&[131, 110, 2, 0, 44, 1]);
        assert!(is_small(term));
        assert_eq!(signed_value(term), 300);

        // A negative magnitude that fits a small.
        let (_h, _a, term) = decode(&[131, 110, 1, 1, 5]);
        assert_eq!(term, make_small(-5));

        // Zero, with a (stripped) redundant high digit.
        let (_h, _a, term) = decode(&[131, 110, 2, 0, 0, 0]);
        assert_eq!(term, make_small(0));

        // An empty magnitude is zero too.
        let (_h, _a, term) = decode(&[131, 110, 0, 0]);
        assert_eq!(term, make_small(0));

        // 12345678901234567890123 — too large for a small, decodes to a bignum whose
        // ten base-256 bytes become two `ErtsDigit`s.
        let (_h, _a, term) = decode(&[
            131, 110, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        assert!(!is_small(term));
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);

        // `LARGE_BIG_EXT` encodes the same value with a 32-bit byte count.
        let (_h, _a, term) = decode(&[
            131, 111, 0, 0, 0, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2,
        ]);
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);

        // The sign byte is a boolean: any non-zero value means negative
        // (`if (xsgn) d = -d;`, `big.c:3446-3448`), so `2` negates 5.
        let (_h, _a, term) = decode(&[131, 110, 1, 2, 5]);
        assert_eq!(term, make_small(-5));
        // A zero magnitude is zero whatever the sign byte is (`external.c:4484-4486`).
        let (_h, _a, term) = decode(&[131, 110, 1, 5, 0]);
        assert_eq!(term, make_small(0));

        // More base-256 bytes than `BIG_ARITY_MAX * sizeof(ErtsDigit)` (524280) is a
        // hard wire bound (`external.c:6146`) — reported as `badarg` like every other
        // decode failure (the oracle answers `badarg`).
        assert_bad_arg(&[131, 111, 0, 8, 0, 25]);
    }

    #[test]
    fn large_big_accepts_a_byte_count_above_the_digit_count() {
        // The `LARGE_BIG_EXT` bound is a *byte* count (`BIG_ARITY_MAX * sizeof(ErtsDigit)`,
        // i.e. up to 65535 `ErtsDigit`s), so 65536 base-256 bytes = 8192 digits is legal.
        let mut image = vec![131, 111, 0, 1, 0, 0, 0]; // LARGE_BIG_EXT, 65536 bytes, +
        image.extend(std::iter::repeat(0x37).take(65_536));
        let mut heap = Heap::new(8193 + 64).expect("a word heap for the digits");
        let mut atoms = AtomTable::new();
        let term = decode_term(&image, &mut heap, &mut atoms).expect("inside the wire bound");
        let (negative, digits) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(digits.len(), 8192);
        assert!(digits.iter().all(|&digit| digit == 0x3737_3737_3737_3737));
    }

    #[test]
    fn external_identifiers_carry_their_fields() {
        // `NEW_PID_EXT`.
        let mut image = vec![131, 88];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        image.extend_from_slice(&0x0506_0708u32.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (0x0102_0304, 0x0506_0708, 9));

        // `PID_EXT` (legacy): an 8-bit creation.
        let mut image = vec![131, 103];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&1u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        image.push(3);
        let (_h, atoms, term) = decode(&image);
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (1, 2, 3));

        // `NEW_PORT_EXT` (decode-only; the encoder never emits it).
        let mut image = vec![131, 89];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&7u32.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (7, 9));

        // `V4_PORT_EXT`: an 8-byte id — the form the C encoder always emits.
        let mut image = vec![131, 120];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0000_0007_0000_0001u64.to_be_bytes());
        image.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (0x0000_0007_0000_0001, 9));

        // `V4_PORT_EXT` with the maximum 64-bit id.
        let mut image = vec![131, 120];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&u64::MAX.to_be_bytes());
        image.extend_from_slice(&u32::MAX.to_be_bytes());
        let (_h, _a, term) = decode(&image);
        let (number, creation) = (external_port_parts(term).unwrap().1, external_port_parts(term).unwrap().2);
        assert_eq!((number, creation), (u64::MAX, u32::MAX));

        // A `V4_PORT_EXT` whose id field is truncated is malformed.
        assert_bad_arg(&[131, 120, 119, 2, 110, 49, 0, 0, 0, 0]);

        // `PORT_EXT` (legacy): a full 32-bit id, 8-bit creation (`get_uint32` +
        // `get_int8`, `external.c:4702-4712`).
        let mut image = vec![131, 102];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0007_0001u32.to_be_bytes());
        image.push(3);
        let (_h, _a, term) = decode(&image);
        assert_eq!(external_port_parts(term).unwrap().1, 0x0007_0001);
        assert_eq!(external_port_parts(term).unwrap().2, 3);

        // A `PORT_EXT` whose 32-bit id field is truncated is malformed (the legacy
        // form is 4 id bytes + 1 creation byte, not 2 + 1).
        assert_bad_arg(&[131, 102, 119, 2, 110, 49, 0, 7, 3]);
        // A legacy creation above 3 is not a tiny creation (`external.c:4709`).
        assert_bad_arg(&[131, 102, 119, 2, 110, 49, 0, 0, 0, 1, 4]);
        // `NEWER_REFERENCE_EXT`.
        let mut image = vec![131, 90, 0, 2];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&9u32.to_be_bytes());
        image.extend_from_slice(&1u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        let (_h, atoms, term) = decode(&image);
        let (node, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!(ids, &[1, 2]);
        assert_eq!(creation, 9);

        // `NEW_REFERENCE_EXT`: 8-bit creation, tiny creation required, and an
        // 18-bit-bounded first id.
        let mut image = vec![131, 114, 0, 1];
        image.extend_from_slice(&NODE_N1);
        image.push(3);
        image.extend_from_slice(&0x0001_2345u32.to_be_bytes());
        let (_h, _a, term) = decode(&image);
        let (_, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(ids, &[0x0001_2345]);
        assert_eq!(creation, 3);

        // `REFERENCE_EXT` (legacy): creation 1 is tiny, its 18-bit first id is in range.
        let mut image = vec![131, 101];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0x0001_2345u32.to_be_bytes());
        image.push(1);
        let (_h, _a, term) = decode(&image);
        let (_, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(ids, &[0x0001_2345]);
        assert_eq!(creation, 1);
        // A legacy creation above 3 is rejected by both reference forms
        // (`external.c:4763`, `:4777`).
        assert_bad_arg(&[131, 101, 119, 2, 110, 49, 0, 0, 0, 1, 7]);
        assert_bad_arg(&[131, 114, 0, 1, 119, 2, 110, 49, 5, 0, 0, 0, 1]);
        // The first id of both legacy reference forms is bounded by `MAX_REFERENCE`
        // (`1 << 18` = 262144, `erl_term.h:771`; `external.c:4757`/`:4782`);
        // `NEWER_REFERENCE_EXT` (`90`) carries no such bound.
        assert_bad_arg(&[131, 101, 119, 2, 110, 49, 0, 4, 147, 224, 1]);
        assert_bad_arg(&[131, 114, 0, 1, 119, 2, 110, 49, 0, 0, 4, 147, 224]);
        // An id word with the high bit set: the C's `r0` is a `Uint32`
        // (`external.c:4750`), so the bound is an *unsigned* comparison and
        // `0xDEADBEEF` is out of range too (the oracle agrees: `badarg`).
        assert_bad_arg(&[131, 101, 119, 2, 110, 49, 222, 173, 190, 239, 1]);
        assert_bad_arg(&[131, 114, 0, 1, 119, 2, 110, 49, 3, 222, 173, 190, 239]);
        // A legacy creation above 3 is rejected for a pid as well.
        assert_bad_arg(&[131, 103, 119, 2, 110, 49, 0, 0, 0, 1, 0, 0, 0, 2, 4]);

        // A node field that is not an atom is malformed.
        assert_bad_arg(&[131, 88, 97, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_bad_arg(&[131, 101, 106, 0, 0, 0, 1, 5]);

        // `NEWER_REFERENCE_EXT` carries at most five ids; an over-long count is a wire
        // bound and therefore `badarg` in the oracle.
        assert_bad_arg(&[131, 90, 0, 6]);
    }

    #[test]
    fn atom_cache_refs_resolve_or_fail() {
        // Without a cache an `ATOM_CACHE_REF` cannot be resolved.
        assert_bad_arg(&[131, 82, 0]);

        // An unset slot is malformed input.
        let cache = AtomCache::new();
        let bytes = [131u8, 82, 3];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&bytes).unwrap().with_atom_cache(&cache);
        let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A set slot yields the atom it recorded.
        let mut cache = AtomCache::new();
        let mut atoms = AtomTable::new();
        let cached = atoms.intern("cached");
        cache.set(3, cached);
        let mut heap = Heap::new(64).unwrap();
        let mut decoder = Decoder::new(&bytes).unwrap().with_atom_cache(&cache);
        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        decoder.finish().unwrap();
        assert_eq!(term, make_atom(cached));
    }

    #[test]
    fn truncation_at_every_offset_is_bad_arg() {
        // `<<"hi">>` and `<<1:3>>` exercise the binary / bit-binary length+payload
        // headers as well.
        for image in [
            &OK_17[..],
            &LIST_123[..],
            &MAP_A_1[..],
            &[131, 109, 0, 0, 0, 2, b'h', b'i'],
            &[131, 77, 0, 0, 0, 1, 3, 0b0010_0000],
        ] {
            for len in 0..image.len() {
                let mut heap = Heap::new(256).unwrap();
                let mut atoms = AtomTable::new();
                let result = decode_term(&image[..len], &mut heap, &mut atoms);
                assert!(
                    matches!(result, Err(BeamError::BadArg)),
                    "offset {len} of {image:?} must be rejected, got {result:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Binaries and bitstrings
    // -----------------------------------------------------------------------

    /// The oracle images, captured from OTP 29.1.1 with
    /// `io:format("~w~n", [term_to_binary(T)])`.
    #[test]
    fn binary_and_bit_binary_decode() {
        // `<<"hi">>`: `[109][len:4 BE][data]`.
        let (_h, _a, term) = decode(&[131, 109, 0, 0, 0, 2, 104, 105]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 16);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[104u8, 105][..], 0, 16));

        // `<<>>`: a zero-length binary is legal (only `BIT_BINARY_EXT` rejects 0).
        let (_h, _a, term) = decode(&[131, 109, 0, 0, 0, 0]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 0);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[][..], 0, 0));

        // `<<1:3>>`: `[77][len:4 BE][bits:1][data]` — the length precedes the
        // trailing-bit count, and the byte is `0b0010_0000` (MSB-first).
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 3, 0b0010_0000]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 3);
        assert_eq!(
            bitstring_bytes(term).unwrap(),
            (&[0b0010_0000u8][..], 0, 3)
        );

        // `<<5:12>>`: 2 bytes, 4 significant bits in the last one.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 4, 0x00, 0x50]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 12);
        assert_eq!(bitstring_bytes(term).unwrap(), (&[0x00u8, 0x50][..], 0, 12));

        // `<<3:13>>`.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 5, 0x00, 0x18]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 13);

        // `<<1:1>>`: `len = 1`, `bits = 1` ⇒ 1 bit (the C allows `size_in_bits < 8`
        // after the trailing-bit adjustment, as long as `len != 0`).
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 1, 0b1000_0000]);
        assert_eq!(bitstring_size_bits(term).unwrap(), 1);

        // A trailing-bit count of 8 is accepted for backwards compatibility and is
        // equivalent to the byte-aligned form.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 2, 8, b'h', b'i']);
        assert_eq!(bitstring_size_bits(term).unwrap(), 16);

        // The trailing (non-significant) bits of the last byte are normalised to zero.
        let (_h, _a, term) = decode(&[131, 77, 0, 0, 0, 1, 3, 0b0011_1111]);
        assert_eq!(
            bitstring_bytes(term).unwrap(),
            (&[0b0010_0000u8][..], 0, 3)
        );
    }

    #[test]
    fn malformed_binaries_and_bit_strings_are_bad_arg() {
        // A trailing-bit count outside `1..=8`.
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 0, 0x00]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 9, 0x00]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 1, 255, 0x00]);
        // Zero bytes: `len * 8 < 8`.
        assert_bad_arg(&[131, 77, 0, 0, 0, 0, 8]);
        // A length that lies past the end of the buffer (payload missing) — rejected
        // *before* anything is allocated.
        assert_bad_arg(&[131, 109, 0, 0, 0, 2, b'h']);
        assert_bad_arg(&[131, 109, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_bad_arg(&[131, 77, 0, 0, 0, 2, 3, b'h']);
        // A bit-binary without even its trailing-bit byte.
        assert_bad_arg(&[131, 77, 0, 0, 0, 1]);
    }

    #[test]
    fn unknown_and_unsupported_tags() {
        // A byte that is not a tag at all.
        assert_bad_arg(&[131, 200]);
        assert_bad_arg(&[131, 0]);
        // `VERSION_MAGIC` is not a term tag.
        assert_bad_arg(&[131, 131]);

        // Obsolete/dead tags are *malformed* (`BadArg`), exactly as the C rejects
        // them: `FUN_EXT` (removed in OTP 23) and `NEW_CACHE` (dead, `#if 0`).
        assert_bad_arg(&[131, Tag::Fun.as_byte()]);
        assert_bad_arg(&[131, Tag::NewCache.as_byte()]);

        // `RECORD_EXT` (67) is implemented now, so a bare tag byte is a *truncation*
        // (the count, flags and fields are missing) — BadArg, never `unsupported`.
        assert_bad_arg(&[131, Tag::Record.as_byte()]);

        // `LOCAL_EXT` (121) is honoured now as the `[local]` wrapper, so a bare tag
        // byte is a *truncation* (the 4-byte hash and the term are missing) — BadArg,
        // never `unsupported`.
        assert_bad_arg(&[131, Tag::Local.as_byte()]);

        // `DIST_HEADER` (68) is a distribution-transport wrapper, never a term: the C
        // term decoder rejects it as malformed, so it is BadArg, never `unsupported`.
        assert_bad_arg(&[131, Tag::DistHeader.as_byte()]);

        // `NEW_FUN_EXT`/`EXPORT_EXT` are implemented now, so a bare tag byte is a
        // *truncation* (the C would also run off the end and error), never
        // `unsupported`.
        assert_bad_arg(&[131, Tag::NewFun.as_byte()]);
        assert_bad_arg(&[131, Tag::Export.as_byte()]);

        // `COMPRESSED` is *covered* now, but only as the top-level wrapper: as a bare
        // tag (a 2-byte image, below the 6-byte minimum) it is malformed, and nested
        // inside a term it is malformed too — never `unsupported`, never a term.
        assert_bad_arg(&[131, 80]);
        assert_bad_arg(&[131, 104, 1, 80]);
    }

    // -----------------------------------------------------------------------
    // `LOCAL_EXT` (121) — the `term_to_binary/2 [local]` wrapper (not a term tag)
    // -----------------------------------------------------------------------

    #[test]
    fn local_ext_wrapper_consumes_hash_and_decodes_the_next_term() {
        // `term_to_binary({ok,17}, [local])`:
        // `[VERSION_MAGIC][LOCAL_EXT][node-hash:4 BE][<term>]`. The node hash is
        // opaque to the decoder (verification needs the node's `local_node_hash`), so
        // any 4 bytes must be accepted and skipped.
        let mut image = vec![131u8, 121, 0xDE, 0xAD, 0xBE, 0xEF];
        image.extend_from_slice(&[104, 2, 119, 2, 111, 107, 97, 17]); // {ok,17}

        let (_h, atoms, term) = decode(&image);
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(
            atoms.name(atom_val(tuple_element(term, 0).unwrap())),
            Some("ok")
        );
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));

        // The wrapper is honoured inline too: nested as a sub-term of a tuple, the
        // 4-byte hash is consumed and the following object is the element (the C does
        // the same with `goto continue_this_obj`).
        let nested = [131u8, 104, 1, 121, 0, 0, 0, 0, 97, 7];
        let (_h, _a, term) = decode(&nested);
        assert_eq!(tuple_arity(term).unwrap(), 1);
        assert_eq!(tuple_element(term, 0).unwrap(), make_small(7));
    }

    #[test]
    fn local_ext_body_decoder_path() {
        // A body (no version byte) that starts with the `[local]` wrapper.
        let body = [121u8, 0xAA, 0xBB, 0xCC, 0xDD, 97, 5];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new_body(&body);
        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        assert_eq!(term, make_small(5));
        assert_eq!(decoder.remaining(), 0);
        decoder.finish().unwrap();
    }

    #[test]
    fn local_ext_truncation_is_bad_arg() {
        // The complete image `[VERSION_MAGIC][LOCAL_EXT][hash:4][{ok,17}]`.
        let full = [131u8, 121, 0, 0, 0, 1, 104, 2, 119, 2, 111, 107, 97, 17];

        // Truncation at *every* prefix is a typed BadArg, never a panic.
        for len in 0..full.len() {
            assert_bad_arg(&full[..len]);
        }

        // The specific boundaries the wrapper introduces: no hash, a partial hash and
        // a full hash with no term following.
        assert_bad_arg(&[131, 121]);
        assert_bad_arg(&[131, 121, 0, 0, 0]);
        assert_bad_arg(&[131, 121, 0, 0, 0, 0]);

        // A body decoder sees the same truncations (no version byte to absorb).
        for len in 0..(full.len() - 1) {
            let body = &full[1..][..len];
            let mut heap = Heap::new(64).unwrap();
            let mut atoms = AtomTable::new();
            let mut decoder = Decoder::new_body(body);
            let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "got {err:?} for {body:?}");
        }
    }

    // -----------------------------------------------------------------------
    // `[local]` internal-node marker (`NIL_EXT`, 106) in an identifier's node field
    // -----------------------------------------------------------------------

    /// The `[local]` **internal-node marker**: `LOCAL_EXT` sets
    /// `ERTS_DIST_EXT_INTERNAL_NC` (`external.c:5250-5254`), `dec_sysname` then accepts
    /// a `NIL_EXT` node field (`external.c:3114-3122`) and `dec_is_this_node` returns
    /// true for it unconditionally (`external.c:3123-3129`) — with no installed local
    /// node — so the identifier demotes to the **internal** form.
    #[test]
    fn local_ext_marker_node_demotes_an_internal_pid_and_port() {
        // `term_to_binary(self(), [local])` shape:
        // `[131][121 LOCAL_EXT][hash:4][88 NEW_PID_EXT][106][num:4][ser:4][cre:4]`.
        let mut pid_image = vec![131u8, 121, 0xDE, 0xAD, 0xBE, 0xEF, 88, 106];
        pid_image.extend_from_slice(&10u32.to_be_bytes()); // number
        pid_image.extend_from_slice(&0u32.to_be_bytes()); // serial
        pid_image.extend_from_slice(&0u32.to_be_bytes()); // creation
        let (_h, _a, term) = decode(&pid_image);
        assert!(is_internal_pid(term), "the marker demotes to an internal pid");
        assert_eq!(pid_number(term), 10);
        assert_eq!(pid_serial(term), 0);

        // `term_to_binary(hd(erlang:ports()), [local])` shape:
        // `[131][121][hash:4][120 V4_PORT_EXT][106][num:8][cre:4]`.
        let mut port_image = vec![131u8, 121, 1, 2, 3, 4, 120, 106];
        port_image.extend_from_slice(&0x1234u64.to_be_bytes());
        port_image.extend_from_slice(&0u32.to_be_bytes());
        let (_h, _a, term) = decode(&port_image);
        assert!(is_internal_port(term), "the marker demotes to an internal port");
        assert_eq!(port_number(term), 0x1234);
    }

    /// A `[local]`-marked reference whose id **fits** the internal form's 60-bit slot
    /// demotes; one that does not (an oracle `make_ref()`, whose id spans all three
    /// number slots) has neither a faithful internal nor a faithful external form —
    /// the marker names no sysname — so it is a typed error, never a term carrying the
    /// `[]` sentinel as its node.
    #[test]
    fn local_ext_marker_reference_demotion_is_representability_gated() {
        // `NEWER_REFERENCE_EXT`: `[90][len = 3][106][creation:4 = 0]` then the numbers
        // `[42, 0, 0]` — id 42, well inside the internal slot.
        let mut small = vec![131u8, 121, 0, 0, 0, 0, 90, 0, 3, 106];
        small.extend_from_slice(&0u32.to_be_bytes());
        small.extend_from_slice(&42u32.to_be_bytes());
        small.extend_from_slice(&0u32.to_be_bytes());
        small.extend_from_slice(&0u32.to_be_bytes());
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let term = decode_term(&small, &mut heap, &mut atoms).unwrap();
        assert_eq!(crate::term::internal_ref_id(term).unwrap(), 42);

        // The oracle's own `term_to_binary(make_ref(), [local])` image (captured from
        // OTP 29.1.1): `90,0,3,106,0,0,0,0` then the numbers `[6585, 0x0F9C0001,
        // 0x95BBF08A]`, whose packed id exceeds the 60-bit internal slot.
        let oracle_ref: [u8; 26] = [
            131, 121, 86, 20, 224, 167, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 25, 185, 15, 156, 0, 1,
            149, 187, 240, 138,
        ];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let err = decode_term(&oracle_ref, &mut heap, &mut atoms).unwrap_err();
        assert!(crate::error::is_unsupported(&err), "got {err:?}");
    }

    /// Marker mode changes only which node **field encoding** is accepted: a
    /// `[local]`-marked identifier whose node field is a *real* sysname atom neither
    /// the marker nor an installed local node keeps the faithful **external** form
    /// (`dec_get_node`, `external.c:3131-3135`), exactly as the C does.
    #[test]
    fn local_ext_marker_mode_keeps_a_real_sysname_node_external() {
        let mut image = vec![131u8, 121, 0, 0, 0, 0, 88];
        image.extend_from_slice(&NODE_N1[..]); // a real node atom, not the marker
        image.extend_from_slice(&3u32.to_be_bytes()); // number
        image.extend_from_slice(&4u32.to_be_bytes()); // serial
        image.extend_from_slice(&5u32.to_be_bytes()); // creation
        let (_h, atoms, term) = decode(&image);
        assert!(!is_internal_pid(term));
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (3, 4, 5));
    }

    /// A truncated `[local]`-marked identifier is a typed `BadArg` at every prefix,
    /// never a panic; and the marker is refused *outside* marker mode, since the C
    /// gates `dec_sysname`'s `NIL_EXT` acceptance on `ERTS_DIST_EXT_INTERNAL_NC`
    /// (`external.c:3114`).
    #[test]
    fn local_ext_marker_truncation_is_bad_arg() {
        let full = [
            131u8, 121, 0, 0, 0, 0, 88, 106, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        for len in 0..full.len() {
            assert_bad_arg(&full[..len]);
        }

        // The same pid with no `LOCAL_EXT` wrapper in front of it.
        assert_bad_arg(&[131, 88, 106, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
    // -----------------------------------------------------------------------
    // `COMPRESSED` (`80`) — the top-level zlib wrapper
    // -----------------------------------------------------------------------

    /// `term_to_binary(lists:duplicate(500, hello), [compressed])` — an oracle image
    /// captured from OTP 29.1.1. Layout: `131, 80, <inner size:4 BE>, <zlib stream>`;
    /// the declared inner size is `3506`, the length of `term_to_binary(List)` minus
    /// its version byte.
    const COMPRESSED_DUP_500_HELLO: [u8; 50] = [
        131, 80, 0, 0, 13, 178, 120, 156, 237, 197, 177, 9, 0, 32, 16, 4, 176, 183, 112, 84, 65,
        228, 192, 210, 197, 29, 192, 53, 44, 146, 38, 169, 106, 247, 244, 57, 146, 45, 73, 146,
        36, 73, 250, 168, 245, 0, 145, 146, 3, 87,
    ];

    #[test]
    fn compressed_top_level_wrapper_is_unwrapped() {
        // The inner body needs 500 cons cells: a heap of 256 words is too small.
        let mut heap = Heap::new(4096).unwrap();
        let mut atoms = AtomTable::new();
        let term = decode_term(&COMPRESSED_DUP_500_HELLO, &mut heap, &mut atoms).unwrap();

        // The inner term is `[hello, hello, …]` — 500 atoms, proper tail.
        let mut count = 0usize;
        let mut cursor = term;
        while is_list(cursor) {
            let head = cons_head(cursor).unwrap();
            assert!(is_atom(head), "expected an atom element, got {head:?}");
            assert_eq!(atoms.name(atom_val(head)), Some("hello"));
            cursor = cons_tail(cursor).unwrap();
            count += 1;
        }
        assert!(is_nil(cursor), "the inner list must be proper");
        assert_eq!(count, 500);
    }

    #[test]
    fn hand_built_compressed_image_decodes() {
        // The inner body of `{ok,17}` — the `OK_17` image without its version byte.
        let body: &[u8] = &[104, 2, 119, 2, 111, 107, 97, 17];
        let zlib = crate::compress::compress(body, 6).unwrap();
        let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
        image.extend_from_slice(&(body.len() as u32).to_be_bytes());
        image.extend_from_slice(&zlib);

        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let term = decode_term(&image, &mut heap, &mut atoms).unwrap();
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(
            atoms.name(atom_val(tuple_element(term, 0).unwrap())),
            Some("ok")
        );
        assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
    }

    #[test]
    fn compressed_negative_cases_are_bad_arg() {
        let image = COMPRESSED_DUP_500_HELLO;

        // Fewer than 6 bytes: the C does not even take the `COMPRESSED` branch
        // (it needs the tag plus the 4-byte size, `external.c:1759`), so the `80` is
        // treated as an ordinary — and therefore invalid — term tag.
        assert_bad_arg(&[131, 80]);
        assert_bad_arg(&[131, 80, 0, 0, 13]);

        // A declared size that disagrees with the actual decompressed length.
        let mut wrong_size = image.to_vec();
        wrong_size[2..6].copy_from_slice(&1u32.to_be_bytes());
        assert_bad_arg(&wrong_size);

        // A corrupt Adler-32 trailer: zlib rejects the stream.
        let mut corrupt = image.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        assert_bad_arg(&corrupt);

        // A payload that is not a zlib stream at all.
        assert_bad_arg(&[131, 80, 0, 0, 0, 10, 1, 2, 3, 4, 5, 6]);

        // Every truncation past the 6-byte header must be rejected, never a panic.
        for len in 6..image.len() {
            assert_bad_arg(&image[..len]);
        }
    }
    #[test]
    fn version_byte_is_required_and_checked() {
        // Missing (empty input).
        assert_bad_arg(&[]);
        // Missing (a bare body).
        assert_bad_arg(&[97, 17]);
        // Wrong version.
        assert_bad_arg(&[130, 97, 17]);

        assert!(matches!(Decoder::new(&[]).err(), Some(BeamError::BadArg)));
        assert!(matches!(
            Decoder::new(&[130]).err(),
            Some(BeamError::BadArg)
        ));
        assert!(Decoder::new(&[131]).is_ok());
    }

    #[test]
    fn cursor_position_remaining_and_finish() {
        let bytes = [131u8, 97, 17, 0];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&bytes).unwrap();
        // The version byte is consumed by `new`.
        assert_eq!(decoder.position(), 1);
        assert_eq!(decoder.remaining(), 3);

        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        assert_eq!(term, make_small(17));
        assert_eq!(decoder.position(), 3);
        assert_eq!(decoder.remaining(), 1);

        // The trailing byte is rejected by `finish`.
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // `decode_term` reports the same trailing-byte error.
        assert_bad_arg(&bytes);
    }

    #[test]
    fn new_body_has_no_version_byte() {
        let body = [97u8, 17];
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new_body(&body);
        assert_eq!(decoder.position(), 0);
        assert_eq!(decoder.decode(&mut heap, &mut atoms).unwrap(), make_small(17));
        assert_eq!(decoder.remaining(), 0);
        decoder.finish().unwrap();

        // A versioned image fed to `new_body` sees `131`, which is not a tag.
        let versioned = [131u8, 97, 17];
        let mut decoder = Decoder::new_body(&versioned);
        let err = decoder.decode(&mut heap, &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    // -----------------------------------------------------------------------
    // Local-node identifier demotion (`Decode` side of the A.5 round-trip)
    // -----------------------------------------------------------------------
    //
    // With the local node identity installed (`Decoder::with_local_node`) an
    // incoming external identifier whose node/creation name *this* node is
    // materialized as the **internal** form (internal pid/port immediate, M1
    // internal reference) instead of an external thing — the C's
    // `dec_is_this_node` branches (`external.c:3175-3182` pid, `:4721` port,
    // `:4821-4862` ref). Without an identity every one of these images keeps
    // decoding to the external form, so the seam is inert by default.

    /// Interns the `\"n1\"` node atom and decodes `bytes` with that node as the local
    /// node (creation `creation`), returning the term and the table it points into.
    fn decode_with_local_node(bytes: &[u8], creation: u32) -> (Heap, AtomTable, Eterm) {
        let mut heap = Heap::new(256).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let node = atoms.intern("n1");
        let term = decode_term_with_local_node(bytes, &mut heap, &mut atoms, node, creation, None)
            .expect("valid local image decodes");
        (heap, atoms, term)
    }

    #[test]
    fn a_local_pid_and_port_demote_to_the_internal_form() {
        use beam_term::prelude::{
            is_internal_pid, is_internal_port, pid_number, pid_serial, port_number,
        };

        // `NEW_PID_EXT` naming the local node: number 0x01020304, serial 0x05060708.
        let mut pid = vec![131, 88];
        pid.extend_from_slice(&NODE_N1);
        pid.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        pid.extend_from_slice(&0x0506_0708u32.to_be_bytes());
        pid.extend_from_slice(&9u32.to_be_bytes());
        let (_h, _a, term) = decode_with_local_node(&pid, 9);
        assert!(is_internal_pid(term), "expected an internal pid, got {term:?}");
        // The wire reads number then serial; the internal form recovers both.
        assert_eq!(pid_number(term), 0x0102_0304);
        assert_eq!(pid_serial(term), 0x0506_0708);

        // `V4_PORT_EXT` naming the local node: a full 64-bit id.
        let mut port = vec![131, 120];
        port.extend_from_slice(&NODE_N1);
        port.extend_from_slice(&0x0000_0007_0000_0001u64.to_be_bytes());
        port.extend_from_slice(&5u32.to_be_bytes());
        let (_h, _a, term) = decode_with_local_node(&port, 5);
        assert!(is_internal_port(term), "expected an internal port, got {term:?}");
        assert_eq!(port_number(term), 0x0000_0007_0000_0001);

        // The legacy `PID_EXT` (tiny 8-bit creation) demotes too, and its field
        // order (number then serial) is preserved.
        let mut legacy = vec![131, 103];
        legacy.extend_from_slice(&NODE_N1);
        legacy.extend_from_slice(&1u32.to_be_bytes());
        legacy.extend_from_slice(&2u32.to_be_bytes());
        legacy.push(3);
        let (_h, _a, term) = decode_with_local_node(&legacy, 3);
        assert!(is_internal_pid(term));
        assert_eq!((pid_number(term), pid_serial(term)), (1, 2));
    }

    #[test]
    fn a_local_reference_demotes_to_the_internal_form() {
        use crate::term::internal_ref_id;

        // `NEWER_REFERENCE_EXT` carrying three numbers is the id their pack yields.
        let mut image = vec![131, 90, 0, 3];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&7u32.to_be_bytes());
        image.extend_from_slice(&1u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        image.extend_from_slice(&3u32.to_be_bytes());
        let (_h, _a, term) = decode_with_local_node(&image, 7);
        assert_eq!(
            internal_ref_id(term).expect("internal reference"),
            crate::ref_wire::ref_numbers_to_id(&[1, 2, 3])
        );

        // A legacy `REFERENCE_EXT` (101) with `r0 = 42` is the id 42 (a single number
        // is zero-padded to the three the internal form packs).
        let mut legacy = vec![131, 101];
        legacy.extend_from_slice(&NODE_N1);
        legacy.extend_from_slice(&42u32.to_be_bytes());
        legacy.push(1);
        let (_h, _a, term) = decode_with_local_node(&legacy, 1);
        assert_eq!(internal_ref_id(term).unwrap(), 42);
    }

    #[test]
    fn a_local_reference_round_trips_every_id_magnitude() {
        use crate::term::internal_ref_id;

        for id in [
            0u64,
            1,
            42,
            1 << 18,
            1 << 32,
            1 << 40,
            1 << 45,
            (1u64 << 59) - 1,
        ] {
            // Build the image from the id's own wire numbers, so the round-trip is
            // exercised across the whole 60-bit id space.
            let numbers = crate::ref_wire::ref_id_to_numbers(id);
            let mut image = vec![131, 90, 0, 3];
            image.extend_from_slice(&NODE_N1);
            image.extend_from_slice(&0u32.to_be_bytes()); // creation
            for number in numbers {
                image.extend_from_slice(&number.to_be_bytes());
            }
            let (_h, _a, term) = decode_with_local_node(&image, 0);
            assert_eq!(internal_ref_id(term).unwrap(), id, "id = {id}");
        }
    }

    #[test]
    fn a_local_reference_too_large_for_the_internal_form_stays_external() {
        use crate::term::internal_ref_id;

        // The gate fixture: a local-node `NEWER_REFERENCE_EXT` (90) whose third
        // number is `0xA02C04B4`, so the packed id is far past `MAX_SMALL` — the
        // internal M1 reference's slot cannot hold it. Such an image must still
        // decode (a well-formed wire reference is never an error) to the faithful
        // **external** form.
        let image: Vec<u8> = vec![
            131, 90, 0, 3, 119, 13, b'n', b'o', b'n', b'o', b'd', b'e', b'@', b'n', b'o', b'h',
            b'o', b's', b't', 0, 0, 0, 0, 0, 2, 35, 202, 52, 184, 0, 8, 160, 44, 4, 180,
        ];
        let mut heap = Heap::new(64).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let node = atoms.intern("nonode@nohost");
        let term = decode_term_with_local_node(&image, &mut heap, &mut atoms, node, 0, None)
            .expect("a well-formed local wire reference decodes");

        // Not the internal form…
        assert!(
            internal_ref_id(term).is_err(),
            "an unrepresentable id must not become an internal reference"
        );
        // …the external reference instead, with every number and the creation kept.
        let (ref_node, ids, creation) = external_ref_parts(term).expect("external reference");
        assert_eq!(atoms.name(atom_val(ref_node)), Some("nonode@nohost"));
        assert_eq!(ids, [0x0002_23CA, 0x34B8_0008, 0xA02C_04B4]);
        assert_eq!(creation, 0);

        // …and it re-encodes byte-for-byte: the external form is the faithful
        // representation of this image.
        assert_eq!(crate::encode_term(term, &atoms).unwrap(), image);
    }

    #[test]
    fn a_local_new_reference_ext_too_large_for_the_internal_form_stays_external() {
        use crate::term::internal_ref_id;

        // The same rule for the compact `NEW_REFERENCE_EXT` (114) form: `r0` is 0,
        // the following two numbers pack an id past `MAX_SMALL`
        // (`0xB8000000 << 32 | 0x0223CA34 & REF1_NUM_MASK`), so the internal form is
        // out of reach. The image re-encodes as the modern 3-number `90` form.
        let mut image = vec![131, 114, 0, 3];
        image.extend_from_slice(&NODE_N1);
        image.push(0); // creation
        image.extend_from_slice(&0u32.to_be_bytes()); // r0
        image.extend_from_slice(&0x0223_CA34u32.to_be_bytes());
        image.extend_from_slice(&0xB800_0000u32.to_be_bytes());

        let mut heap = Heap::new(64).expect("tiny heap allocation");
        let mut atoms = AtomTable::new();
        let node = atoms.intern("n1");
        let term = decode_term_with_local_node(&image, &mut heap, &mut atoms, node, 0, None)
            .expect("a well-formed local wire reference decodes");
        assert!(internal_ref_id(term).is_err());

        let (ref_node, ids, creation) = external_ref_parts(term).expect("external reference");
        assert_eq!(atoms.name(atom_val(ref_node)), Some("n1"));
        assert_eq!(ids, [0, 0x0223_CA34, 0xB800_0000]);
        assert_eq!(creation, 0);

        let mut expected = vec![131, 90, 0, 3];
        expected.extend_from_slice(&NODE_N1);
        expected.extend_from_slice(&0u32.to_be_bytes()); // creation
        for id in ids {
            expected.extend_from_slice(&id.to_be_bytes());
        }
        assert_eq!(crate::encode_term(term, &atoms).unwrap(), expected);
    }

    #[test]
    fn a_local_reference_with_a_representable_id_still_demotes() {
        use crate::term::internal_ref_id;

        // The boundary: a packed id exactly at `MAX_SMALL` is still representable, so
        // the demotion happens — the gate is the small-int range, not a narrower one.
        let max = beam_term::prelude::MAX_SMALL as u64;
        let numbers = crate::ref_wire::ref_id_to_numbers(max);
        let mut image = vec![131, 90, 0, 3];
        image.extend_from_slice(&NODE_N1);
        image.extend_from_slice(&0u32.to_be_bytes()); // creation
        for number in numbers {
            image.extend_from_slice(&number.to_be_bytes());
        }
        let (_h, _a, term) = decode_with_local_node(&image, 0);
        assert_eq!(internal_ref_id(term).unwrap(), max);
    }

    #[test]
    fn a_compressed_local_image_demotes_through_the_entry_point() {
        use beam_term::prelude::is_internal_pid;

        // A `COMPRESSED` image whose inner body is a local `NEW_PID_EXT`: the identity
        // must be installed on the compressed-body path too (it is not a bare
        // `decode` of the inflated bytes).
        let mut body = vec![88];
        body.extend_from_slice(&NODE_N1);
        body.extend_from_slice(&7u32.to_be_bytes());
        body.extend_from_slice(&8u32.to_be_bytes());
        body.extend_from_slice(&4u32.to_be_bytes());
        let zlib = crate::compress::compress(&body, 6).unwrap();
        let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
        image.extend_from_slice(&(body.len() as u32).to_be_bytes());
        image.extend_from_slice(&zlib);

        let (_h, _a, term) = decode_with_local_node(&image, 4);
        assert!(is_internal_pid(term), "expected an internal pid, got {term:?}");
    }

    #[test]
    fn without_an_identity_the_external_form_is_kept() {
        // The very same local-node images decode to the *external* things when no
        // identity is installed — the seam is inert by default and decoding is
        // unchanged.
        let mut pid = vec![131, 88];
        pid.extend_from_slice(&NODE_N1);
        pid.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        pid.extend_from_slice(&0x0506_0708u32.to_be_bytes());
        pid.extend_from_slice(&9u32.to_be_bytes());
        let (_h, atoms, term) = decode(&pid);
        let (node, number, serial, creation) = external_pid_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, serial, creation), (0x0102_0304, 0x0506_0708, 9));

        let mut port = vec![131, 120];
        port.extend_from_slice(&NODE_N1);
        port.extend_from_slice(&7u64.to_be_bytes());
        port.extend_from_slice(&5u32.to_be_bytes());
        let (_h, _a, term) = decode(&port);
        let (node, number, creation) = external_port_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((number, creation), (7, 5));

        let mut reference = vec![131, 90, 0, 1];
        reference.extend_from_slice(&NODE_N1);
        reference.extend_from_slice(&7u32.to_be_bytes());
        reference.extend_from_slice(&1u32.to_be_bytes());
        let (_h, _a, term) = decode(&reference);
        let (node, ids, creation) = external_ref_parts(term).unwrap();
        assert_eq!(atoms.name(atom_val(node)), Some("n1"));
        assert_eq!((ids, creation), (&[1u32][..], 7));
    }

    // -----------------------------------------------------------------------
    // The body-form entry points: `decode_term_body` / `decode_term_body_used`
    // -----------------------------------------------------------------------

    /// A body-form image of a repetitive term whose zlib stream does shrink it, so
    /// `encode_term_body_compressed` emits the `[COMPRESSED_EXT]…` wrapper.
    fn compressed_body_image() -> (Vec<u8>, Vec<u8>) {
        let mut atoms = AtomTable::new();
        // A 500-element list of the same atom compresses well below `len - 5`.
        let hello = make_atom(atoms.intern("hello"));
        let mut heap = Heap::new(4096).unwrap();
        let mut list = NIL;
        for _ in 0..500 {
            list = heap.cons(hello, list).unwrap();
        }
        let body = crate::encode::encode_term_body(list, &atoms).unwrap();
        let image = crate::encode::encode_term_body_compressed(list, &atoms, 6).unwrap();
        assert_eq!(image[0], COMPRESSED_EXT, "the body form carries no version byte");
        assert_eq!(
            u32::from_be_bytes([image[1], image[2], image[3], image[4]]) as usize,
            body.len()
        );
        (body, image)
    }

    #[test]
    fn is_compressed_body_requires_the_tag_and_five_bytes() {
        assert!(is_compressed_body(&[COMPRESSED_EXT, 0, 0, 0, 1]));
        // Four bytes: the 4-byte size field is not complete yet.
        assert!(!is_compressed_body(&[COMPRESSED_EXT, 0, 0, 0]));
        // The *versioned* form does not start with the tag.
        assert!(!is_compressed_body(&[VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 1]));
        // A plain body (a tag that is not `COMPRESSED`) is not compressed.
        assert!(!is_compressed_body(&[97, 7]));
        assert!(!is_compressed_body(&[]));
    }

    #[test]
    fn decode_term_body_decodes_the_plain_body_form() {
        // `encode_term_body` is the symmetric producer (no version byte).
        let mut atoms = AtomTable::new();
        let term = make_atom(atoms.intern("hello"));
        let body = crate::encode::encode_term_body(term, &atoms).unwrap();
        assert_eq!(body[0], 119, "SMALL_ATOM_UTF8_EXT");

        let mut heap = Heap::new(64).unwrap();
        let mut atoms2 = AtomTable::new();
        let (decoded, used) = decode_term_body_used(&body, &mut heap, &mut atoms2).unwrap();
        assert_eq!(used, body.len());
        assert_eq!(atoms2.name(atom_val(decoded)), Some("hello"));
    }

    #[test]
    fn decode_term_body_unwraps_the_compressed_body_form() {
        let (body, image) = compressed_body_image();
        // The plain body still decodes, and the compressed body decodes to the same
        // term — the producer/consumer pair is symmetric with no manual `131` prepend.
        let mut plain_heap = Heap::new(8192).unwrap();
        let mut plain_atoms = AtomTable::new();
        let (t_plain, used_plain) =
            decode_term_body_used(&body, &mut plain_heap, &mut plain_atoms).unwrap();
        assert_eq!(used_plain, body.len());

        let mut heap = Heap::new(8192).unwrap();
        let mut atoms = AtomTable::new();
        let (t_compressed, used) = decode_term_body_used(&image, &mut heap, &mut atoms).unwrap();
        // The `used` count is the 5-byte body header plus the whole zlib stream.
        assert_eq!(used, image.len());

        // Structural equality through the safe accessors (atoms by name).
        assert_eq!(is_list(t_plain), is_list(t_compressed));
        assert_eq!(is_list(t_compressed), true);
    }

    #[test]
    fn decode_term_body_rejects_a_lying_declared_size() {
        let (_body, mut image) = compressed_body_image();
        // Bump the declared inner size by one: the inflated length no longer matches
        // (`external.c:1807-1810`) — BadArg, never a partially decoded term.
        let declared = u32::from_be_bytes([image[1], image[2], image[3], image[4]]);
        image[4] = (declared.wrapping_add(1) & 0xFF) as u8;
        let mut heap = Heap::new(8192).unwrap();
        let mut atoms = AtomTable::new();
        assert!(matches!(
            decode_term_body(&image, &mut heap, &mut atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn decode_term_body_rejects_a_corrupt_zlib_stream() {
        let (_body, mut image) = compressed_body_image();
        // Flip a byte in the deflate stream: zlib fails => BadArg.
        let last = image.len() - 1;
        image[last] ^= 0xFF;
        let mut heap = Heap::new(8192).unwrap();
        let mut atoms = AtomTable::new();
        assert!(decode_term_body(&image, &mut heap, &mut atoms).is_err());
    }

    #[test]
    fn decode_term_body_rejects_a_truncated_body() {
        // A body whose term is truncated (a tuple announcing two elements, one given).
        assert!(matches!(
            decode_term_body(&[104, 2, 97, 1], &mut Heap::new(64).unwrap(), &mut AtomTable::new()),
            Err(BeamError::BadArg)
        ));
    }
