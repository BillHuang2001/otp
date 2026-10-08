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

    //! Byte-exact encoder tests: every tag family is asserted against the exact
    //! wire bytes, including the width-policy boundaries. Terms are built with
    //! `beam_alloc::Heap` + the [`crate::term`] constructors.

    use beam_alloc::Heap;
    use beam_common::{BeamError, Result};
    use beam_term::prelude::{
        make_atom, make_internal_pid, make_pid_data, make_small, Atom, NIL,
    };

    use super::*;
    use crate::error::is_unsupported;
    use crate::term::{
        make_big, make_external_pid, make_external_port, make_external_ref, make_flatmap,
        make_float,
    };

    /// A heap large enough for every term a test builds (tests pass a bigger one
    /// when they need it). Shared with the [`fun`](crate::encode::fun) submodule's
    /// tests.
    pub(crate) fn new_heap(words: usize) -> Heap {
        Heap::new(words).expect("the carrier allocator never fails for a test heap")
    }

    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    pub(crate) fn assert_system_limit<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::SystemLimit) => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    /// Builds a proper list from `elems` (head first) on `heap`.
    fn build_list(heap: &mut Heap, elems: &[i64]) -> Eterm {
        let mut list = NIL;
        for &value in elems.iter().rev() {
            list = heap.cons(make_small(value), list).expect("list fits");
        }
        list
    }

    #[test]
    fn small_integers_follow_the_width_policy() {
        let atoms = AtomTable::new();
        let cases: &[(i64, &[u8])] = &[
            (0, &[97, 0]),
            (1, &[97, 1]),
            (255, &[97, 255]),
            (256, &[98, 0, 0, 1, 0]),
            (-1, &[98, 255, 255, 255, 255]),
            (i64::from(i32::MAX), &[98, 127, 255, 255, 255]),
            (i64::from(i32::MIN), &[98, 128, 0, 0, 0]),
            // Just past the signed 32-bit range ⇒ bignum, magnitude little-endian.
            (i64::from(i32::MAX) + 1, &[110, 4, 0, 0, 0, 0, 128]),
            (i64::from(i32::MIN) - 1, &[110, 4, 1, 1, 0, 0, 128]),
            (1i64 << 40, &[110, 6, 0, 0, 0, 0, 0, 0, 1]),
        ];
        for (value, expected) in cases {
            let bytes = encode_term(make_small(*value), &atoms).unwrap();
            assert_eq!(bytes[0], 131);
            assert_eq!(&bytes[1..], *expected, "small {value} mis-encoded");
        }
    }

    #[test]
    fn bignum_terms_use_small_big_and_strip_high_zeros() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);

        let t = make_big(&mut heap, false, &[0x05]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 0, 5]);

        let t = make_big(&mut heap, true, &[0x0A]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 1, 10]);

        // The magnitude is little-endian, so a high-order zero **digit** contributes
        // only its (stripped) trailing byte.
        let t = make_big(&mut heap, false, &[5, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![110, 1, 0, 5]);

        // A zero digit *below* the most significant one still contributes all eight of
        // its bytes (`big_to_bytes`): the value is 2^128 + 5, i.e. 17 significant
        // base-256 bytes.
        let t = make_big(&mut heap, false, &[5, 0, 1]).unwrap();
        let mut expected = vec![110u8, 17, 0, 5];
        expected.extend(core::iter::repeat_n(0u8, 15));
        expected.push(1);
        assert_eq!(encode_term_body(t, &atoms).unwrap(), expected);

        // An all-zero magnitude coincides with the small integer 0 on the wire.
        let t = make_big(&mut heap, false, &[0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![97, 0]);
        let t = make_big(&mut heap, false, &[0, 0]).unwrap();
        assert_eq!(encode_term_body(t, &atoms).unwrap(), vec![97, 0]);
    }

    #[test]
    fn digit_bytes_are_little_endian_with_a_short_top_digit() {
        assert_eq!(digits_to_le_bytes(&[]), Vec::<u8>::new());
        assert_eq!(digits_to_le_bytes(&[1]), vec![1]);
        assert_eq!(digits_to_le_bytes(&[0x0102]), vec![0x02, 0x01]);
        assert_eq!(
            digits_to_le_bytes(&[0x0102, 0x03]),
            vec![0x02, 0x01, 0, 0, 0, 0, 0, 0, 0x03]
        );
        assert_eq!(digits_to_le_bytes(&[0, 0]), vec![0; 8 + 1]);
        assert_eq!(digits_to_le_bytes(&[u64::MAX]), vec![0xFF; 8]);
    }

    #[test]
    fn bignum_with_at_least_256_bytes_uses_large_big() {
        let atoms = AtomTable::new();
        // 32 words of `0x01` bytes = 256 significant base-256 bytes.
        let digits = vec![0x0101_0101_0101_0101u64; 32];
        let mut heap = new_heap(4096);
        let t = make_big(&mut heap, false, &digits).unwrap();

        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(bytes[0], 111);
        assert_eq!(&bytes[1..5], &[0, 0, 1, 0]); // 256, big-endian u32
        assert_eq!(bytes[5], 0); // sign
        assert_eq!(&bytes[6..], &vec![1u8; 256][..]);
        assert_eq!(bytes.len(), 1 + 4 + 1 + 256);
    }

    #[test]
    fn atoms_use_small_utf8_by_default() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("ok");
        assert_eq!(
            encode_term(make_atom(a), &atoms).unwrap(),
            vec![131, 119, 2, 111, 107]
        );
    }

    #[test]
    fn atoms_can_be_encoded_as_latin1() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("ok");
        let mut encoder = Encoder::new(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(make_atom(a)).unwrap();
        assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);
    }

    #[test]
    fn minor_version_selects_the_atom_and_float_flavours() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let mut heap = new_heap(64);
        let float = make_float(&mut heap, 3.14).unwrap();

        // The default (V2) is unchanged: `SMALL_ATOM_UTF8_EXT` + `NEW_FLOAT_EXT`.
        assert_eq!(
            encode_term(make_atom(ok), &atoms).unwrap(),
            vec![131, 119, 2, 111, 107]
        );
        assert_eq!(encode_term(float, &atoms).unwrap()[1], NEW_FLOAT_EXT);

        // V1 pins `ATOM_EXT` but keeps `NEW_FLOAT_EXT`.
        let mut encoder = Encoder::new(&atoms);
        encoder.set_minor_version(MinorVersion::V1);
        encoder.encode(make_atom(ok)).unwrap();
        assert_eq!(encoder.finish(), vec![131, 100, 0, 2, 111, 107]);
        let mut encoder = Encoder::new(&atoms).with_minor_version(MinorVersion::V1);
        encoder.encode(float).unwrap();
        assert_eq!(encoder.finish()[1], NEW_FLOAT_EXT);

        // V0 additionally uses the 31-byte `FLOAT_EXT` text form.
        let mut encoder = Encoder::new(&atoms).with_minor_version(MinorVersion::V0);
        encoder.encode(float).unwrap();
        let bytes = encoder.finish();
        assert_eq!(bytes[1], FLOAT_EXT);
        assert_eq!(bytes.len(), 2 + float::FLOAT_EXT_LEN);
        assert_eq!(&bytes[2..], b"3.14000000000000012434e+00\0\0\0\0\0");

        // The convenience entry point agrees with the builder.
        assert_eq!(
            encode_term_with_minor_version(make_atom(ok), &atoms, MinorVersion::V0).unwrap(),
            vec![131, 100, 0, 2, 111, 107]
        );
        assert_eq!(
            encode_term_with_minor_version(make_atom(ok), &atoms, MinorVersion::V2).unwrap(),
            vec![131, 119, 2, 111, 107]
        );
    }

    #[test]
    fn long_and_multibyte_atom_names_pick_the_wide_tags() {
        let mut atoms = AtomTable::new();

        // 255 one-byte characters: the largest `SMALL_ATOM_UTF8_EXT`.
        let a = atoms.intern(&"a".repeat(255));
        let bytes = encode_term_body(make_atom(a), &atoms).unwrap();
        assert_eq!(&bytes[..2], &[119, 255]);
        assert_eq!(bytes.len(), 2 + 255);

        // 200 two-byte characters (400 UTF-8 bytes) force `ATOM_UTF8_EXT`, but are
        // only 200 characters, so they stay within the OTP limit.
        let e_acute = atoms.intern(&"\u{00e9}".repeat(200));
        let bytes = encode_term_body(make_atom(e_acute), &atoms).unwrap();
        assert_eq!(&bytes[..3], &[118, 0x01, 0x90]); // 400 = 0x0190
        assert_eq!(bytes.len(), 3 + 400);

        // The same name in Latin-1 is 200 one-byte code points ⇒ `SMALL_ATOM_EXT`.
        let mut encoder = Encoder::new_body(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(make_atom(e_acute)).unwrap();
        let latin = encoder.finish();
        assert_eq!(&latin[..2], &[115, 200]);
        assert_eq!(latin.len(), 2 + 200);
        assert_eq!(latin[2], 0xE9);
    }

    #[test]
    fn over_long_atom_names_are_a_system_limit() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern(&"a".repeat(256));
        assert_system_limit(encode_term(make_atom(a), &atoms));
    }

    #[test]
    fn non_latin1_atom_under_latin1_is_unsupported() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("\u{03bb}"); // Greek small letter lambda, U+03BB
        let mut encoder = Encoder::new(&atoms);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        let err = encoder.encode(make_atom(a)).unwrap_err();
        assert!(is_unsupported(&err));

        // The same atom is fine as UTF-8 (λ is two UTF-8 bytes).
        assert_eq!(
            encode_term_body(make_atom(a), &atoms).unwrap(),
            vec![119, 2, 0xCE, 0xBB]
        );
    }

    #[test]
    fn unknown_atom_index_is_bad_arg() {
        let atoms = AtomTable::new();
        // An atom index the table never interned has no name to render.
        let ghost = make_atom(Atom::from_raw(0xDEAD_BEEF));
        assert_bad_arg(encode_term(ghost, &atoms));
    }

    #[test]
    fn nil_encodes_to_106() {
        let atoms = AtomTable::new();
        assert_eq!(encode_term(NIL, &atoms).unwrap(), vec![131, 106]);
        assert_eq!(encode_term_body(NIL, &atoms).unwrap(), vec![106]);
    }

    #[test]
    fn proper_byte_lists_use_string_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        let list = build_list(&mut heap, &[1, 2, 3]);
        assert_eq!(encode_term_body(list, &atoms).unwrap(), vec![107, 0, 3, 1, 2, 3]);
    }

    #[test]
    fn a_list_with_a_non_byte_element_uses_list_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        // 256 is a small int but not a byte, so the whole list is `LIST_EXT`.
        let list = build_list(&mut heap, &[1, 256]);
        assert_eq!(
            encode_term_body(list, &atoms).unwrap(),
            vec![108, 0, 0, 0, 2, 97, 1, 98, 0, 0, 1, 0, 106]
        );
    }

    #[test]
    fn improper_lists_use_list_ext_and_encode_the_tail() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        let list = heap.cons(make_small(1), make_small(2)).unwrap();
        assert_eq!(
            encode_term_body(list, &atoms).unwrap(),
            vec![108, 0, 0, 0, 1, 97, 1, 97, 2]
        );
    }

    #[test]
    fn byte_list_of_65535_uses_string_ext() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(200_000);
        let mut list = NIL;
        for _ in 0..65_535 {
            list = heap.cons(make_small(0), list).unwrap();
        }
        let bytes = encode_term_body(list, &atoms).unwrap();
        assert_eq!(bytes[0], 107);
        assert_eq!(&bytes[1..3], &[0xFF, 0xFF]);
        assert_eq!(bytes.len(), 3 + 65_535);
    }

    #[test]
    fn byte_list_of_65536_uses_list_ext_not_an_error() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(200_000);
        let mut list = NIL;
        for _ in 0..65_536 {
            list = heap.cons(make_small(0), list).unwrap();
        }
        let bytes = encode_term_body(list, &atoms).unwrap();
        assert_eq!(bytes[0], 108);
        assert_eq!(&bytes[1..5], &[0, 1, 0, 0]); // 65536, big-endian u32
        // Each element is the small int 0 → [97, 0]; the tail is NIL (106).
        assert_eq!(bytes.len(), 5 + 65_536 * 2 + 1);
        assert_eq!(*bytes.last().unwrap(), 106);
    }

    #[test]
    fn tuple_arity_255_and_256_pick_the_right_tag() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(1024);

        let t = heap.tuple(&vec![make_small(0); 255]).unwrap();
        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(&bytes[..2], &[104, 255]);
        assert_eq!(bytes.len(), 2 + 255 * 2);

        let t = heap.tuple(&vec![make_small(0); 256]).unwrap();
        let bytes = encode_term_body(t, &atoms).unwrap();
        assert_eq!(&bytes[..5], &[105, 0, 0, 1, 0]);
        assert_eq!(bytes.len(), 5 + 256 * 2);
    }

    #[test]
    fn floats_use_new_float_ext_and_ieee754_bits() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
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
            let t = make_float(&mut heap, value).unwrap();
            let bytes = encode_term_body(t, &atoms).unwrap();
            assert_eq!(bytes[0], 70);
            assert_eq!(&bytes[1..], &value.to_bits().to_be_bytes());
            assert_eq!(bytes.len(), 9);
        }
    }

    #[test]
    fn maps_encode_as_map_ext() {
        let mut atoms = AtomTable::new();
        let mut heap = new_heap(64);

        let empty = make_flatmap(&mut heap, &[]).unwrap();
        assert_eq!(encode_term_body(empty, &atoms).unwrap(), vec![116, 0, 0, 0, 0]);

        let a = make_atom(atoms.intern("a"));
        let one = make_flatmap(&mut heap, &[(a, make_small(1))]).unwrap();
        assert_eq!(
            encode_term_body(one, &atoms).unwrap(),
            vec![116, 0, 0, 0, 1, 119, 1, 97, 97, 1]
        );

        let b = make_atom(atoms.intern("b"));
        let two = make_flatmap(&mut heap, &[(a, make_small(1)), (b, make_small(2))]).unwrap();
        assert_eq!(
            encode_term_body(two, &atoms).unwrap(),
            vec![116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2]
        );
    }

    #[test]
    fn maps_above_the_small_limit_encode_as_map_ext() {
        // A *flat* map of 40 pairs — past `MAP_SMALL_MAP_LIMIT` (32) — still encodes as a
        // single `MAP_EXT` in stored order. (A real > 32-pair *hash map* is a different
        // shape the encoder does not handle; see `tests/map_key_order.rs`.)
        let mut atoms = AtomTable::new();
        let mut heap = new_heap(4 + 2 * 40 + 64);
        let mut pairs = Vec::new();
        for i in 0..40i64 {
            let key = make_atom(atoms.intern(&format!("k{i}")));
            pairs.push((key, make_small(i)));
        }
        let map = make_flatmap(&mut heap, &pairs).unwrap();

        let bytes = encode_term_body(map, &atoms).unwrap();
        assert_eq!(bytes[0], 116); // MAP_EXT
        assert_eq!(u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]), 40);
        // The wire order is the pair order: `K1,V1,…,Kn,Vn` (name then value).
        assert_eq!(&bytes[5..9], &[119, 2, b'k', b'0']); // "k0"
        assert_eq!(bytes[9], 97); // small integer
        assert_eq!(bytes[10], 0);
    }

    #[test]
    fn external_pid_uses_new_pid_ext() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);
        let pid = make_external_pid(&mut heap, node, 1, 2, 3).unwrap();

        let mut expected = vec![88u8];
        expected.extend_from_slice(&[119, 13]);
        expected.extend_from_slice(b"nonode@nohost");
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&2u32.to_be_bytes());
        expected.extend_from_slice(&3u32.to_be_bytes());
        assert_eq!(encode_term_body(pid, &atoms).unwrap(), expected);
    }

    #[test]
    fn internal_pid_uses_local_node_for_new_pid_ext() {
        let mut atoms = AtomTable::new();
        let sysname = atoms.intern("nonode@nohost");
        // `make_pid_data(ser, num)` ⇒ serial 0, process number 10.
        let pid = make_internal_pid(make_pid_data(0, 10));

        // With a local node identity the internal pid becomes a `NEW_PID_EXT`.
        let mut encoder = Encoder::new(&atoms).with_local_node(sysname, 0);
        encoder.encode(pid).unwrap();
        assert_eq!(
            encoder.finish(),
            vec![
                131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115,
                116, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );

        // Without a local node identity an internal id has no wire image.
        let mut bare = Encoder::new(&atoms);
        let err = bare
            .encode(pid)
            .expect_err("an internal pid without a local node is unsupported");
        assert!(is_unsupported(&err));
    }

    #[test]
    fn external_port_uses_v4_port_ext_with_the_full_id() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);

        let expected = |number: u64, creation: u32| {
            let mut e = vec![120u8];
            e.extend_from_slice(&[119, 13]);
            e.extend_from_slice(b"nonode@nohost");
            e.extend_from_slice(&number.to_be_bytes());
            e.extend_from_slice(&creation.to_be_bytes());
            e
        };

        // A small id still uses the 64-bit `V4_PORT_EXT` form (never `NEW_PORT_EXT`).
        let port = make_external_port(&mut heap, node, 0x1234, 5).unwrap();
        assert_eq!(encode_term_body(port, &atoms).unwrap(), expected(0x1234, 5));

        // A 64-bit port id is written in full — never truncated, never a limit.
        let wide = make_external_port(&mut heap, node, 0x1_0000_0000, 5).unwrap();
        assert_eq!(
            encode_term_body(wide, &atoms).unwrap(),
            expected(0x1_0000_0000, 5)
        );

        let max = make_external_port(&mut heap, node, u64::MAX, u32::MAX).unwrap();
        assert_eq!(
            encode_term_body(max, &atoms).unwrap(),
            expected(u64::MAX, u32::MAX)
        );
    }

    #[test]
    fn external_ref_uses_newer_reference_ext() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut heap = new_heap(64);
        let r = make_external_ref(&mut heap, node, &[1, 2, 3], 9).unwrap();

        let mut expected = vec![90u8];
        expected.extend_from_slice(&3u16.to_be_bytes());
        expected.extend_from_slice(&[119, 13]);
        expected.extend_from_slice(b"nonode@nohost");
        expected.extend_from_slice(&9u32.to_be_bytes());
        expected.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(encode_term_body(r, &atoms).unwrap(), expected);
    }

    #[test]
    fn version_magic_is_emitted_only_by_new() {
        let atoms = AtomTable::new();

        let mut head = Encoder::new(&atoms);
        assert_eq!(head.as_bytes(), &[131]);
        assert_eq!(head.len(), 1);
        assert!(!head.is_empty());
        head.encode(NIL).unwrap();
        assert_eq!(head.finish(), vec![131, 106]);

        let mut body = Encoder::new_body(&atoms);
        assert!(body.is_empty());
        assert_eq!(body.len(), 0);
        assert_eq!(body.as_bytes(), &[] as &[u8]);
        body.encode(NIL).unwrap();
        assert_eq!(body.finish(), vec![106]);
    }

    #[test]
    fn nested_terms_encode_recursively() {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut heap = new_heap(256);

        let inner_list = build_list(&mut heap, &[1, 2]);
        let map = make_flatmap(&mut heap, &[(a, make_small(7))]).unwrap();
        let tuple = heap.tuple(&[inner_list, map]).unwrap();

        let mut expected = vec![104u8, 2];
        expected.extend_from_slice(&[107, 0, 2, 1, 2]); // [1, 2]
        expected.extend_from_slice(&[116, 0, 0, 0, 1, 119, 1, 97, 97, 7]); // #{a => 7}
        assert_eq!(encode_term_body(tuple, &atoms).unwrap(), expected);
    }

    /// A test [`AtomCacheWriter`] that caches exactly the atoms in `cached` (in
    /// order, starting at index `start`) and declines every other atom.
    struct TestCache {
        /// The atoms the cache accepts, in index order.
        cached: Vec<Atom>,
        /// The payload index assigned to `cached[0]`.
        start: u8,
        /// Every atom the encoder consulted, in order.
        seen: Vec<Atom>,
    }

    impl TestCache {
        fn new(cached: Vec<Atom>, start: u8) -> TestCache {
            TestCache {
                cached,
                start,
                seen: Vec::new(),
            }
        }
    }

    impl AtomCacheWriter for TestCache {
        fn intern(&mut self, atom: Atom, _atoms: &AtomTable) -> Option<u8> {
            self.seen.push(atom);
            self.cached
                .iter()
                .position(|&cached| cached == atom)
                .map(|slot| self.start + slot as u8)
        }
    }

    #[test]
    fn an_uncached_encoder_is_byte_identical_with_a_declining_cache() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let term = make_atom(ok);

        let plain = encode_term_body(term, &atoms).unwrap();
        assert_eq!(plain, vec![119, 2, 111, 107]);

        // A cache that declines every atom must not move a single byte.
        let mut cache = TestCache::new(Vec::new(), 0);
        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(term).unwrap();
        assert_eq!(encoder.finish(), plain);
        // The cache *was* consulted (and recorded the atom) even though it declined.
        assert_eq!(cache.seen, vec![ok]);
    }

    #[test]
    fn a_cached_atom_writes_atom_cache_ref() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let mut cache = TestCache::new(vec![ok], 3);

        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(make_atom(ok)).unwrap();
        assert_eq!(encoder.finish(), vec![82, 3]);
    }

    #[test]
    fn a_cache_miss_falls_through_to_the_inline_atom() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let other = atoms.intern("other");
        // The cache only takes `other`, so `ok` is written inline.
        let mut cache = TestCache::new(vec![other], 0);

        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(make_atom(ok)).unwrap();
        assert_eq!(encoder.finish(), vec![119, 2, 111, 107]);
    }

    #[test]
    fn cache_indices_may_differ_per_atom() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("a");
        let b = atoms.intern("b");
        let mut cache = TestCache::new(vec![a, b], 10);

        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(make_atom(a)).unwrap();
        encoder.encode(make_atom(b)).unwrap();
        assert_eq!(encoder.finish(), vec![82, 10, 82, 11]);
    }

    #[test]
    fn nested_terms_cache_only_the_atoms_the_cache_takes() {
        let mut atoms = AtomTable::new();
        let cached = atoms.intern("cache_me");
        let inline = atoms.intern("inline_me");
        let mut heap = new_heap(64);
        let tuple = heap
            .tuple(&[make_atom(cached), make_small(1), make_atom(inline)])
            .unwrap();
        let mut cache = TestCache::new(vec![cached], 7);

        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(tuple).unwrap();

        let mut expected = vec![104u8, 3, 82, 7, 97, 1];
        expected.extend_from_slice(&[119, 9]);
        expected.extend_from_slice(b"inline_me");
        assert_eq!(encoder.finish(), expected);
        // The cache saw the inline atom too (it was consulted, it just declined).
        assert_eq!(cache.seen, vec![cached, inline]);
    }

    #[test]
    fn with_atom_cache_composes_with_the_version_byte_and_atom_encoding() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let term = make_atom(ok);

        // Declining cache + Latin-1: identical to an encoder with neither.
        let mut miss = TestCache::new(Vec::new(), 0);
        let mut encoder = Encoder::new(&atoms).with_atom_cache(&mut miss);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(term).unwrap();
        assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);

        // A hit replaces the atom with `[82, index]` (the version byte is untouched
        // and the atom encoding is irrelevant on the cached path).
        let mut hit = TestCache::new(vec![ok], 4);
        let mut encoder = Encoder::new(&atoms).with_atom_cache(&mut hit);
        encoder.set_atom_encoding(AtomEncoding::Latin1);
        encoder.encode(term).unwrap();
        assert_eq!(encoder.finish(), vec![131, 82, 4]);
    }

    #[test]
    fn local_encoding_wraps_the_body_with_the_tag_and_hash() {
        let atoms = AtomTable::new();
        assert_eq!(
            encode_term_local(make_small(17), &atoms, 0xAABB_CCDD).unwrap(),
            vec![131, 121, 0xAA, 0xBB, 0xCC, 0xDD, 97, 17]
        );
    }

    #[test]
    fn local_encoding_only_prepends_the_wrapper() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("a");
        let mut heap = new_heap(256);
        let term = heap.tuple(&[make_atom(a), make_small(1)]).unwrap();

        let body = encode_term_body(term, &atoms).unwrap();
        let mut expected = vec![131u8, 121, 0x01, 0x02, 0x03, 0x04];
        expected.extend_from_slice(&body);
        assert_eq!(
            encode_term_local(term, &atoms, 0x0102_0304).unwrap(),
            expected
        );
    }

    #[test]
    fn encoder_new_and_new_body_produce_the_same_local_wrapper() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("a");
        let mut heap = new_heap(64);
        let list = heap.cons(make_atom(a), NIL).unwrap();

        let body = encode_term_body(list, &atoms).unwrap();
        let mut with_version = Encoder::new(&atoms);
        with_version.encode(list).unwrap();
        let mut without_version = Encoder::new_body(&atoms);
        without_version.encode(list).unwrap();

        // `Encoder::new`'s extra version byte is stripped, so both agree...
        assert_eq!(with_version.finish_local(9), without_version.finish_local(9));
        // ... and both are `[131, 121, hash] ++ body`.
        let mut expected = vec![131u8, 121, 0, 0, 0, 9];
        expected.extend_from_slice(&body);
        assert_eq!(encode_term_local(list, &atoms, 9).unwrap(), expected);
    }

    #[test]
    fn local_encoding_composes_with_the_atom_cache() {
        let mut atoms = AtomTable::new();
        let ok = atoms.intern("ok");
        let mut cache = TestCache::new(vec![ok], 2);

        let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
        encoder.encode(make_atom(ok)).unwrap();
        assert_eq!(encoder.finish_local(1), vec![131, 121, 0, 0, 0, 1, 82, 2]);
    }
