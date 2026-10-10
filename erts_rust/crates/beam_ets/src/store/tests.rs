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

    //! Copy-boundary tests: structural equality with the source, distinct
    //! table-owned storage, and survival of the source heap's storage.
    //!
    //! Term fixtures are built with [`beam_term`] — the frozen representation is
    //! only ever constructed through its helpers, never by hand-rolling bit
    //! patterns beyond the documented thing layouts in the fixture builders below.

    use super::*;
    use beam_alloc::{AllocatorConfig, CarrierAllocator};

    /// A store over a private, small-carrier allocator, so tests never disturb the
    /// process-wide registry's counters.
    fn test_store() -> TermStore {
        let alloc = CarrierAllocator::new(
            AllocatorConfig::builder()
                .name("beam_ets_store_test")
                .carrier_size(64 * 1024)
                .build(),
        )
        .expect("a fresh carrier allocator must be constructible")
        .share();
        TermStore::new(alloc)
    }

    /// Builds a flat map in the ERTS layout (`erl_term.h:1320-1326`):
    /// `[MAP_HEADER_FLATMAP, n, keys_tuple, v0 … v(n-1)]`, with the keys tuple
    /// allocated just before the map head.
    ///
    /// # Safety
    ///
    /// `heap` must have room for `(1 + n)` (keys tuple) `+ (3 + n)` (map) words and
    /// must outlive the returned term.
    unsafe fn build_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let n = pairs.len();
        // SAFETY: `alloc_tuple` hands over `1 + n` exclusively-owned words, all of
        // which are written below before the term escapes.
        let keys = unsafe {
            heap.alloc_tuple(n)
                .expect("heap room for the keys tuple")
        };
        for (i, (key, _)) in pairs.iter().enumerate() {
            // SAFETY: element slot `1 + i` is inside the keys tuple.
            unsafe { *keys.add(1 + i) = *key };
        }
        // SAFETY: `alloc_words` hands over `n + 3` exclusively-owned words, all of
        // which are written below.
        let p = unsafe {
            heap.alloc_words(n + 3)
                .expect("heap room for the flat-map block")
        };
        // SAFETY: the block owns `n + 3` words: `[header, size, keys, v0 … v(n-1)]`.
        unsafe {
            *p = make_header(4, MAP_SUBTAG);
            *p.add(1) = Eterm::from_raw(n);
            *p.add(2) = make_tuple(keys);
            for (i, (_, value)) in pairs.iter().enumerate() {
                *p.add(3 + i) = *value;
            }
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a 64-bit float thing.
    ///
    /// # Safety
    ///
    /// `heap` must have room for 2 words and must outlive the returned term.
    unsafe fn build_float(heap: &mut Heap, value: f64) -> Eterm {
        // SAFETY: 2 exclusively-owned words, both written before the term escapes.
        let p = unsafe { heap.alloc_words(2).expect("heap room for a float") };
        // SAFETY: `p` owns exactly those 2 words.
        unsafe {
            *p = HEADER_FLONUM;
            *p.add(1) = Eterm::from_raw(value.to_bits() as usize);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a one-limb bignum thing (`header + 1 payload word`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 2 words and must outlive the returned term.
    unsafe fn build_bignum(heap: &mut Heap, negative: bool) -> Eterm {
        let subtag = if negative {
            NEG_BIG_SUBTAG
        } else {
            POS_BIG_SUBTAG
        };
        // SAFETY: 2 exclusively-owned words, both written before the term escapes.
        let p = unsafe { heap.alloc_words(2).expect("heap room for a bignum") };
        // SAFETY: `p` owns exactly those 2 words.
        unsafe {
            *p = make_header(1, subtag);
            *p.add(1) = Eterm::from_raw(0x1234_5678_9ABC_DEF0);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds an internal reference thing (`header + 2 payload words`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 3 words and must outlive the returned term.
    unsafe fn build_internal_ref(heap: &mut Heap) -> Eterm {
        // SAFETY: 3 exclusively-owned words, all written before the term escapes.
        let p = unsafe { heap.alloc_words(3).expect("heap room for a ref") };
        // SAFETY: `p` owns exactly those 3 words.
        unsafe {
            *p = make_header(2, REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(0xFFFF_FFFF_0000_0000);
            *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a 16-bit on-heap binary (`[header, size, 1 data word]`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 3 words and must outlive the returned term.
    unsafe fn build_heap_binary(heap: &mut Heap, value: u16) -> Eterm {
        // SAFETY: 3 exclusively-owned words, all written before the term escapes.
        let p = unsafe { heap.alloc_words(3).expect("heap room for a binary") };
        // SAFETY: `p` owns exactly those 3 words.
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16); // bit size
            *p.add(2) = Eterm::from_raw(value as usize);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a **malformed** sub-bitstring thing: `[HEADER_SUB_BITS, base_flags,
    /// start, end, orig]` whose `base_flags` is a **null** pointer and whose bit
    /// window is non-empty (`start = 0`, `end = 4`), so it names no readable payload.
    ///
    /// A well-formed sub-bitstring (and an off-heap refc binary) **is** in the copied
    /// subset — [`TermStore::copy_sub_bits`] re-materialises it as a self-contained
    /// on-heap bitstring (see [`build_refc_binary`]/[`build_non_aligned_sub_bits`] for
    /// the positive fixtures). This shape is the *malformed* one that the null-base
    /// guard rejects with `badarg` instead of dereferencing a null base.
    ///
    /// # Safety
    ///
    /// `heap` must have room for 5 words and must outlive the returned term.
    unsafe fn build_sub_bits(heap: &mut Heap) -> Eterm {
        // SAFETY: 5 exclusively-owned words, all written before the term escapes.
        let p = unsafe { heap.alloc_words(5).expect("heap room for a sub-bits thing") };
        // SAFETY: `p` owns exactly those 5 words.
        unsafe {
            *p = HEADER_SUB_BITS;
            *p.add(1) = Eterm::from_raw(0);
            *p.add(2) = Eterm::from_raw(4);
            *p.add(3) = Eterm::from_raw(16);
            *p.add(4) = make_small(0);
        }
        make_boxed(p as *const Eterm)
    }

    /// `bytes` copied into a fresh **word-aligned** payload of whole `Eterm` words
    /// (zero-padded) — the aligned backing storage an off-heap bitstring's base pointer
    /// needs (`write_refc_binary`'s `debug_assert`). A `Box<[u8]>` would not be
    /// `Eterm`-aligned.
    fn word_aligned_payload(bytes: &[u8]) -> Box<[Eterm]> {
        let word_bytes = core::mem::size_of::<Eterm>();
        let mut payload: Box<[Eterm]> =
            vec![Eterm::from_raw(0); bytes.len().div_ceil(word_bytes).max(1)].into_boxed_slice();
        // SAFETY: `payload` owns at least `bytes.len()` writable bytes (rounded up to
        // whole words, at least one), so the copy stays inside the box.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                payload.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        payload
    }

    /// Builds a well-formed off-heap **refc binary**: the 8-word `[BinRef][ErlSubBits]`
    /// pair ([`write_refc_binary`], root at `hp + ERL_BIN_REF_SIZE`), whose payload is a
    /// [`word_aligned_payload`] of `bytes`. The payload box is returned alongside the
    /// term so the test keeps it alive: the writer stores a raw pointer into it and
    /// `copy_sub_bits` gathers those bytes.
    ///
    /// # Safety
    ///
    /// `heap` must have room for [`ERL_REFC_BITS_SIZE`] (8) words and must outlive the
    /// returned term.
    unsafe fn build_refc_binary(heap: &mut Heap, bytes: &[u8]) -> (Eterm, Box<[Eterm]>) {
        let payload = word_aligned_payload(bytes);
        let base = payload.as_ptr().cast::<u8>();
        // SAFETY: the heap has room for the 8-word pair; `base` names the `Eterm`-aligned
        // payload's first byte, covers `bytes.len() * 8` bits, and outlives the term (the
        // box is handed back to the caller).
        let root = unsafe {
            let hp = heap
                .alloc_words(ERL_REFC_BITS_SIZE)
                .expect("heap room for a refc binary");
            write_refc_binary(
                hp,
                payload.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                base,
                0,
                bytes.len() * 8,
            )
        };
        (root, payload)
    }
    /// The bit window `[start, start + size)` of `base` unpacked into bytes,
    /// most-significant bit of the first byte first — an **independent** expectation for
    /// [`TermStore::copy_sub_bits`], whose own gatherer is a different code path.
    /// [`the_window_expectation_matches_a_hand_computation`] pins this helper against a
    /// literal, so the two cannot be wrong together.
    fn window_bytes(base: &[u8], start: usize, size: usize) -> Vec<u8> {
        let mut out = vec![0u8; size.div_ceil(8)];
        for i in 0..size {
            let bit = (base[(start + i) / 8] >> (7 - ((start + i) % 8))) & 1;
            out[i / 8] |= bit << (7 - (i % 8));
        }
        out
    }

    /// Builds a well-formed `ErlSubBits` over a **word-aligned** payload whose bit window
    /// is **not byte-aligned** (`start = 3`, `size = 13`), so [`TermStore::copy_sub_bits`]
    /// must take its bit-by-bit gather path (not the aligned slice copy) and
    /// re-materialise a bitstring whose size is not a whole number of bytes.
    ///
    /// `payload_bytes` must cover at least the 16 bits the window reads. The payload box
    /// is returned so the test keeps it alive.
    ///
    /// # Safety
    ///
    /// `heap` must have room for [`sub_bits_size_words`] (5) words and must outlive the
    /// returned term.
    unsafe fn build_non_aligned_sub_bits(
        heap: &mut Heap,
        payload_bytes: &[u8],
    ) -> (Eterm, Box<[Eterm]>) {
        /// The window's first bit and its (non-multiple-of-8) size in bits.
        const START_BIT: usize = 3;
        const SIZE_BITS: usize = 13;
        assert!(
            payload_bytes.len() * 8 >= START_BIT + SIZE_BITS,
            "the payload must cover the whole window"
        );
        let payload = word_aligned_payload(payload_bytes);
        let base = payload.as_ptr() as usize;
        // SAFETY: the heap has room for the 5-word `ErlSubBits`; `base` is word-aligned
        // (so its low flag bits are clear) and covers the window; `orig` is ignored by
        // `copy_sub_bits` (it is only ever read through a `BinRef` pair).
        let root = unsafe {
            let hp = heap
                .alloc_words(sub_bits_size_words())
                .expect("heap room for a sub-bits thing");
            write_sub_bits(
                hp,
                Eterm::from_raw(base),
                Eterm::from_raw(START_BIT),
                Eterm::from_raw(START_BIT + SIZE_BITS),
                make_small(0),
            )
        };
        (root, payload)
    }
    /// Asserts that `copy` reproduced the whole thing of `source` word for word
    /// (used for the pointer-free leaves).
    fn assert_copied_verbatim(name: &str, source: Eterm, copy: Eterm) {
        assert_ne!(boxed_val(copy), boxed_val(source), "{name}: must be copied");
        // SAFETY: `source` is a live thing of `header_arity + 1` words, and its copy
        // is a live thing of the same size.
        unsafe {
            let n = header_arity(*boxed_val(source)) + 1;
            for i in 0..n {
                assert_eq!(
                    *boxed_val(copy).add(i),
                    *boxed_val(source).add(i),
                    "{name}: word {i} differs"
                );
            }
        }
    }

    #[test]
    fn immediates_copy_by_value_and_cost_no_words() {
        let mut store = test_store();
        let terms = [
            make_small(0),
            make_small(1234),
            make_small(-7),
            NIL,
            make_atom(Atom::from_raw(3)),
            make_catch(17),
            make_internal_pid(make_pid_data(1, 2)),
            make_internal_port(make_port_data(9)),
        ];
        for t in terms {
            assert_eq!(store.copy(t).expect("an immediate is copyable"), t);
        }
        assert_eq!(store.words(), 0, "immediates consume no store words");
        assert_eq!(store.blocks(), 0, "immediates map no block");
    }

    #[test]
    fn cons_list_copy_is_equal_and_lives_in_table_storage() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(16);
        let three = heap.cons(make_small(3), NIL).unwrap();
        let two_three = heap.cons(make_small(2), three).unwrap();
        let one_two_three = heap.cons(make_small(1), two_three).unwrap();

        let mut store = test_store();
        let copied = store.copy(one_two_three).expect("a list is copyable");

        assert!(is_list(copied));
        assert_ne!(
            list_val(copied),
            list_val(one_two_three),
            "the copy must live in table-owned storage"
        );
        assert!(
            eq(one_two_three, copied, &atoms),
            "the copy must be structurally equal to the source"
        );
        // SAFETY: both lists are live and the store outlives the test.
        unsafe {
            assert_eq!(car(copied), make_small(1));
            assert_eq!(car(cdr(copied)), make_small(2));
            assert_eq!(car(cdr(cdr(copied))), make_small(3));
            assert_eq!(cdr(cdr(cdr(copied))), NIL);
        }
        // 3 cons cells x 2 words.
        assert_eq!(store.words(), 6);
        assert_eq!(store.blocks(), 1);
    }

    #[test]
    fn tuple_and_nested_tuple_copy() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(7), make_small(8)]).unwrap();
        let outer = heap.tuple(&[make_small(1), inner, make_small(9)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(outer).expect("a tuple is copyable");

        assert!(unsafe { is_tuple(copied) });
        assert_ne!(boxed_val(copied), boxed_val(outer));
        assert!(eq(outer, copied, &atoms), "nested tuples copy structurally");
        // SAFETY: `copied` is a live store-owned tuple.
        unsafe {
            assert_eq!(tuple_size(copied), 3);
            assert_eq!(tuple_element(copied, 0), make_small(1));
            assert_eq!(tuple_element(copied, 2), make_small(9));
            let inner_copy = tuple_element(copied, 1);
            assert!(is_tuple(inner_copy));
            assert_ne!(
                boxed_val(inner_copy),
                boxed_val(inner),
                "the inner tuple must be deep-copied, not aliased"
            );
            assert_eq!(tuple_size(inner_copy), 2);
            assert_eq!(tuple_element(inner_copy, 0), make_small(7));
            assert_eq!(tuple_element(inner_copy, 1), make_small(8));
        }
        // outer (4 words) + inner (3 words).
        assert_eq!(store.words(), 7);
    }

    #[test]
    fn flatmap_copy_round_trips_the_erts_layout() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("a");
        let b = atoms.intern("b");
        let mut heap = Heap::new(64);
        // SAFETY: the heap has room (keys tuple 3 words + map block 5 words) and
        // outlives the map.
        let map = unsafe {
            build_flatmap(
                &mut heap,
                &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
            )
        };

        let mut store = test_store();
        let copied = store.copy(map).expect("a flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        assert_ne!(boxed_val(copied), boxed_val(map));
        // SAFETY: both maps are live across the whole test.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            // The header and the raw size word are reproduced verbatim.
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 2, "the raw pair count is preserved");
            // The keys tuple is deep-copied into table storage.
            let keys = *dst.add(2);
            assert!(is_tuple(keys));
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
            assert_eq!(tuple_size(keys), 2);
            assert_eq!(tuple_element(keys, 0), make_atom(a));
            assert_eq!(tuple_element(keys, 1), make_atom(b));
            // The values follow the keys slot.
            assert_eq!(*dst.add(3), make_small(1));
            assert_eq!(*dst.add(4), make_small(2));
        }
        // map block `n + 3` = 5 words + keys tuple `1 + 2` = 3 words.
        assert_eq!(store.words(), 8);
    }

    #[test]
    fn empty_flatmap_copy_keeps_its_three_word_block() {
        let mut heap = Heap::new(16);
        // SAFETY: the heap has room (keys tuple 1 word + map block 3 words).
        let map = unsafe { build_flatmap(&mut heap, &[]) };

        let mut store = test_store();
        let copied = store.copy(map).expect("an empty flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        // SAFETY: both maps are live.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 0);
            // The zero-arity keys tuple is still deep-copied.
            let keys = *dst.add(2);
            assert!(is_tuple(keys));
            assert_eq!(tuple_size(keys), 0);
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
        }
        // map 3 words + keys tuple 1 word.
        assert_eq!(store.words(), 4);
    }

    #[test]
    fn flatmap_copy_sizes_from_the_size_word_not_the_header_arity() {
        // A flat-map head whose header arity field is 0 — the shape
        // `beam_term::compare`'s own tests build — still carries its pair count in
        // the size word. Sizing the copy from `map_header_arity` would truncate the
        // block to 3 words and then drop both values.
        let mut heap = Heap::new(32);
        // SAFETY: the heap has room for the keys tuple (3 words) and the block
        // (5 words), and outlives the map.
        let map = unsafe {
            let keys = heap.alloc_tuple(2).expect("heap room for the keys tuple");
            *keys.add(1) = make_small(7);
            *keys.add(2) = make_small(8);
            let p = heap.alloc_words(5).expect("heap room for the map block");
            *p = make_header(0, HAMT_SUBTAG_HEAD_FLATMAP); // arity field is 0 ...
            *p.add(1) = Eterm::from_raw(2); // ... yet there are two pairs
            *p.add(2) = make_tuple(keys);
            *p.add(3) = make_small(10);
            *p.add(4) = make_small(20);
            make_boxed(p as *const Eterm)
        };

        let mut store = test_store();
        let copied = store.copy(map).expect("a flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        // SAFETY: both maps are live for the rest of the test.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 2);
            let keys = *dst.add(2);
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
            assert_eq!(tuple_size(keys), 2);
            assert_eq!(tuple_element(keys, 0), make_small(7));
            assert_eq!(tuple_element(keys, 1), make_small(8));
            assert_eq!(*dst.add(3), make_small(10));
            assert_eq!(*dst.add(4), make_small(20));
        }
        // map block `n + 3` = 5 words + keys tuple 3 words.
        assert_eq!(store.words(), 8);
    }

    #[test]
    fn pointer_free_leaves_are_copied_verbatim() {
        let mut heap = Heap::new(64);
        // SAFETY: the heap has room for every fixture below and outlives them.
        let (float, pos_big, neg_big, internal_ref, binary) = unsafe {
            (
                build_float(&mut heap, 1.5),
                build_bignum(&mut heap, false),
                build_bignum(&mut heap, true),
                build_internal_ref(&mut heap),
                build_heap_binary(&mut heap, 0xBEEF),
            )
        };

        let mut store = test_store();
        let float_copy = store.copy(float).expect("a float is copyable");
        assert_copied_verbatim("float", float, float_copy);
        let pos_big_copy = store.copy(pos_big).expect("a bignum is copyable");
        assert_copied_verbatim("pos_big", pos_big, pos_big_copy);
        let neg_big_copy = store.copy(neg_big).expect("a negative bignum is copyable");
        assert_copied_verbatim("neg_big", neg_big, neg_big_copy);
        let ref_copy = store.copy(internal_ref).expect("a ref is copyable");
        assert_copied_verbatim("internal_ref", internal_ref, ref_copy);
        let binary_copy = store.copy(binary).expect("a heap bitstring is copyable");
        assert_copied_verbatim("heap_bitstring", binary, binary_copy);

        // The copies keep their exact classification (and the binary its bit size).
        // SAFETY: every copy is a live store-owned term.
        unsafe {
            assert!(is_float(float_copy));
            assert!(is_big(pos_big_copy));
            assert!(is_big(neg_big_copy));
            assert!(is_internal_ref(ref_copy));
            assert!(is_bitstring(binary_copy));
            assert!(is_binary(binary_copy));
            assert_eq!(bitstring_size(binary_copy), 16);
        }
        // 2 + 2 + 2 + 3 + 3 words.
        assert_eq!(store.words(), 12);
    }

    #[test]
    fn copy_survives_clobbering_and_dropping_the_source_heap() {
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(11), make_small(12)]).unwrap();
        let outer = heap.tuple(&[inner, make_small(99)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(outer).expect("a tuple is copyable");

        // Rewind the source arena and write different values over the exact words
        // the source tuples occupied, clobbering them in place.
        heap.reset();
        let _scratch = heap.tuple(&[make_small(-1), make_small(-2)]).unwrap();
        assert_eq!(heap.used(), 3, "the clobber re-used the inner tuple's words");

        // SAFETY: `copied` is store-owned and the store is alive.
        unsafe {
            assert_eq!(tuple_size(copied), 2);
            assert_eq!(tuple_element(copied, 1), make_small(99));
            let inner_copy = tuple_element(copied, 0);
            assert_eq!(tuple_element(inner_copy, 0), make_small(11));
            assert_eq!(tuple_element(inner_copy, 1), make_small(12));
        }

        // Dropping the source arena must not affect the copy either.
        drop(heap);
        // SAFETY: as above.
        unsafe {
            assert_eq!(tuple_element(copied, 1), make_small(99));
        }
    }

    #[test]
    fn block_growth_is_non_relocating() {
        const N: i64 = 64;
        let mut heap = Heap::new(4 * N as usize);
        let sources: Vec<Eterm> = (0..N)
            .map(|i| {
                heap.tuple(&[make_small(i), make_small(i * 10)])
                    .expect("heap room for the source tuples")
            })
            .collect();

        let mut store = test_store();
        let copies: Vec<Eterm> = sources
            .iter()
            .map(|&t| store.copy(t).expect("a tuple is copyable"))
            .collect();

        // Each copy is a 3-word tuple: 192 words, well past `BLOCK_WORDS` (64), so
        // the store must have grown past its first block.
        assert!(
            store.blocks() > 1,
            "expected block growth, got {} block(s)",
            store.blocks()
        );
        assert_eq!(store.words(), 3 * N as usize);

        // Every copy landed in distinct storage...
        let mut addresses: Vec<usize> = copies.iter().map(|&c| boxed_val(c) as usize).collect();
        addresses.sort_unstable();
        let before = addresses.len();
        addresses.dedup();
        assert_eq!(before, addresses.len(), "two copies share storage");

        // ...and every earlier root still reads back its original value, which is
        // what proves the earlier blocks were never moved by the later allocations.
        for (i, &c) in copies.iter().enumerate() {
            // SAFETY: every `c` is store-owned and the store is alive.
            unsafe {
                assert_eq!(tuple_size(c), 2);
                assert_eq!(tuple_element(c, 0), make_small(i as i64), "root {i}");
                assert_eq!(tuple_element(c, 1), make_small(i as i64 * 10), "root {i}");
            }
        }
    }

    #[test]
    fn empty_store_reports_no_words_and_no_blocks() {
        let store = TermStore::with_default_allocator();
        assert_eq!(store.words(), 0);
        assert_eq!(store.blocks(), 0);
    }

    #[test]
    fn dropping_a_populated_store_frees_every_block() {
        let alloc = CarrierAllocator::new(
            AllocatorConfig::builder()
                .name("beam_ets_store_drop_test")
                .carrier_size(64 * 1024)
                .build(),
        )
        .expect("a fresh carrier allocator must be constructible")
        .share();
        let mut heap = Heap::new(64);
        let tail = heap.cons(NIL, NIL).unwrap();
        let list = heap.cons(make_small(1), tail).unwrap();

        {
            let mut store = TermStore::new(alloc.clone());
            store.copy(list).expect("a list is copyable");
            assert!(store.blocks() > 0);
            assert!(
                alloc.stats().live_bytes > 0,
                "the store's block must be live while the store lives"
            );
        }
        assert_eq!(
            alloc.stats().live_bytes,
            0,
            "dropping the store must free every block it owns"
        );
    }

    /// The `badarg` path for a **malformed** `ErlSubBits`.
    ///
    /// A well-formed sub-bitstring is copyable (it is re-materialised on-heap — see
    /// [`sub_bitstring_copy_is_self_contained`]); this fixture's base pointer is null
    /// while its bit window is non-empty, so there is no readable payload. `copy`
    /// rejects it through the null-base guard in [`TermStore::copy_sub_bits`] rather
    /// than dereferencing null — the typed `badarg` the oracle cannot produce, since
    /// it cannot represent such a term at all.
    #[test]
    fn malformed_sub_bitstring_is_rejected_with_badarg() {
        let mut heap = Heap::new(16);
        // SAFETY: the heap has room for the 5-word sub-bits thing and outlives it.
        let sub_bits = unsafe { build_sub_bits(&mut heap) };
        let mut store = test_store();
        // `BeamError` has no `PartialEq`, so match the variant.
        assert!(matches!(store.copy(sub_bits), Err(BeamError::BadArg)));
        assert_eq!(store.words(), 0, "a rejected term allocates nothing");
        assert_eq!(store.blocks(), 0);
    }

    // --- per-object reclamation -------------------------------------------------

    /// `free` reports exactly the words a copy consumed, and a second `free` of the
    /// same root is a no-op that never underflows the live-word counter.
    #[test]
    fn free_reclaims_every_run_of_a_nested_copy_without_underflowing() {
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(7), make_small(8)]).unwrap();
        let outer = heap.tuple(&[make_small(1), inner, make_small(9)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(outer).expect("a tuple is copyable");
        // outer (4 words) + inner (3 words).
        assert_eq!(store.words(), 7);

        assert_eq!(
            store.free(copied),
            7,
            "every run of the object is reclaimed"
        );
        assert_eq!(store.words(), 0, "the live-word count returns to zero");
        assert_eq!(
            store.free(copied),
            0,
            "a second free of the same root reclaims nothing"
        );
        assert_eq!(store.words(), 0);
        // Only the *words* were reclaimed: the block is still mapped, so the store
        // still holds (and reports) reclaimable storage.
        assert!(store.memory() > 0);
    }

    #[test]
    fn freeing_a_copied_list_reclaims_every_cons_cell() {
        let mut heap = Heap::new(16);
        let three = heap.cons(make_small(3), NIL).unwrap();
        let two = heap.cons(make_small(2), three).unwrap();
        let one = heap.cons(make_small(1), two).unwrap();

        let mut store = test_store();
        let copied = store.copy(one).expect("a list is copyable");
        // 3 cons cells x 2 words.
        assert_eq!(store.words(), 6);

        assert_eq!(store.free(copied), 6);
        assert_eq!(store.words(), 0);
        assert_eq!(store.free(copied), 0);
    }

    #[test]
    fn freeing_an_immediate_or_a_foreign_term_is_a_no_op() {
        let mut store = test_store();
        let copied = store
            .copy(make_small(42))
            .expect("an immediate is copyable");
        assert_eq!(copied, make_small(42), "immediates copy by value");

        assert_eq!(store.free(copied), 0);
        assert_eq!(store.free(NIL), 0, "a root the store never copied");
        assert_eq!(store.free(make_small(-1)), 0);
        assert_eq!(store.free(make_atom(Atom::from_raw(3))), 0);
        assert_eq!(store.words(), 0, "no word accounting moved");
        assert_eq!(store.blocks(), 0);
        assert_eq!(store.memory(), 0);
    }

    /// Freed runs are reused by a later `copy` — and only by a later one, so every
    /// live term survives untouched.
    #[test]
    fn freed_runs_are_reused_only_by_later_copies() {
        let atoms = AtomTable::new();
        const N: i64 = 32;
        let mut heap = Heap::new(4 * N as usize);
        let sources: Vec<Eterm> = (0..N)
            .map(|i| {
                heap.tuple(&[make_small(i), make_small(i * 10)])
                    .expect("heap room for the source tuples")
            })
            .collect();

        let mut store = test_store();
        let copies: Vec<Eterm> = sources
            .iter()
            .map(|&t| store.copy(t).expect("a tuple is copyable"))
            .collect();

        let live_before = store.words();
        let blocks_before = store.blocks();
        let memory_before = store.memory();
        assert_eq!(live_before, 3 * N as usize);
        assert!(
            blocks_before > 1,
            "the store must have grown past one block"
        );

        // Free every other object; the odd ones must stay live and readable.
        let mut freed = 0;
        let mut freed_roots: Vec<usize> = Vec::new();
        for (i, &c) in copies.iter().enumerate() {
            if i % 2 == 0 {
                freed += store.free(c);
                freed_roots.push(c.raw());
            }
        }
        assert_eq!(freed, 3 * (N / 2) as usize);
        assert_eq!(store.words(), live_before - freed);
        assert_eq!(
            store.blocks(),
            blocks_before,
            "reclaiming words must not shrink the block chain"
        );
        assert_eq!(store.memory(), memory_before);

        // The live (odd) objects are untouched by the frees.
        for i in (1..N as usize).step_by(2) {
            assert!(
                eq(sources[i], copies[i], &atoms),
                "live object {i} was disturbed by the frees"
            );
        }

        // Now copy the freed objects' sources again. Every run is reused, so the
        // roots land exactly on the freed storage and neither the block chain nor
        // the held memory grows. (The freed roots themselves — every even-indexed
        // `copies[i]` — must not be read from here on: their storage is being handed
        // out again.)
        let recopies: Vec<Eterm> = (0..N as usize)
            .step_by(2)
            .map(|i| store.copy(sources[i]).expect("a tuple is copyable"))
            .collect();
        for (k, &r) in recopies.iter().enumerate() {
            let source_index = 2 * k;
            assert!(
                freed_roots.contains(&r.raw()),
                "recopy {k} did not reuse freed storage (root {:#x})",
                r.raw()
            );
            assert!(
                eq(sources[source_index], r, &atoms),
                "recopy {k} is not structurally equal to its source"
            );
        }
        assert_eq!(store.blocks(), blocks_before, "reuse mapped no new block");
        assert_eq!(store.memory(), memory_before, "reuse held no new memory");
        assert_eq!(
            store.words(),
            live_before,
            "the recopies cost exactly the freed words"
        );

        // Every live root reads back its original values: nothing was overwritten.
        for i in (1..N as usize).step_by(2) {
            // SAFETY: `copies[i]` is store-owned and the store is alive.
            unsafe {
                assert_eq!(tuple_size(copies[i]), 2);
                assert_eq!(tuple_element(copies[i], 0), make_small(i as i64));
                assert_eq!(tuple_element(copies[i], 1), make_small(i as i64 * 10));
            }
        }
        for &r in &recopies {
            // SAFETY: `r` is store-owned and the store is alive.
            unsafe {
                assert_eq!(tuple_size(r), 2);
            }
        }
    }

    #[test]
    fn reset_releases_every_block_and_the_store_stays_usable() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(32);
        let source = heap.tuple(&[make_small(1), make_small(2)]).unwrap();

        let mut store = test_store();
        let first = store.copy(source).expect("a tuple is copyable");
        let second = store.copy(source).expect("a tuple is copyable");
        assert!(store.memory() > 0);
        assert!(store.blocks() > 0);
        assert_eq!(store.words(), 6);

        store.reset();
        assert_eq!(store.memory(), 0, "every block is released");
        assert_eq!(store.blocks(), 0);
        assert_eq!(store.words(), 0);
        // The pre-reset roots are no longer tracked, so freeing them is a no-op
        // (they must be freed *before* the store hands their address out again).
        assert_eq!(store.free(first), 0);
        assert_eq!(store.free(second), 0);

        // ...and the store is immediately usable again.
        let third = store.copy(source).expect("a tuple is copyable");
        assert!(store.memory() > 0);
        assert_eq!(store.words(), 3);
        assert!(eq(source, third, &atoms));
    }

    #[test]
    fn reset_returns_the_blocks_to_the_allocator() {
        let alloc = CarrierAllocator::new(
            AllocatorConfig::builder()
                .name("beam_ets_store_reset_test")
                .carrier_size(64 * 1024)
                .build(),
        )
        .expect("a fresh carrier allocator must be constructible")
        .share();
        let mut heap = Heap::new(16);
        let source = heap.tuple(&[make_small(1), make_small(2)]).unwrap();

        let mut store = TermStore::new(alloc.clone());
        store.copy(source).expect("a tuple is copyable");
        assert!(store.memory() > 0);
        assert!(
            alloc.stats().live_bytes > 0,
            "the store's block must be live from the allocator's point of view"
        );

        store.reset();
        assert_eq!(store.memory(), 0);
        assert_eq!(
            alloc.stats().live_bytes,
            0,
            "reset must free every block it owns"
        );
    }

    /// A copy rejected part-way through hands its partial runs back, so a later copy
    /// of the same shape reuses them instead of mapping another block.
    ///
    /// The rejected inner term is the **malformed** `ErlSubBits` fixture (a null base
    /// with a non-empty window — [`build_sub_bits`]), which hits the null-base guard
    /// in [`TermStore::copy_sub_bits`]. The outer tuple has already been allocated
    /// when the rejection happens (the elements are copied before the block is
    /// reserved), so the rollback path is exactly what is under test.
    #[test]
    fn a_rejected_nested_term_leaks_no_words_and_leaves_them_reusable() {
        let mut heap = Heap::new(32);
        // SAFETY: the heap has room for the 5-word sub-bits thing and outlives it.
        let sub_bits = unsafe { build_sub_bits(&mut heap) };
        let outer = heap.tuple(&[make_small(1), sub_bits]).unwrap();

        let mut store = test_store();
        assert!(matches!(store.copy(outer), Err(BeamError::BadArg)));
        assert_eq!(
            store.words(),
            0,
            "a rejected copy returns the runs it had already allocated"
        );

        // The rolled-back run is exactly reused by the next copy of the same size,
        // so no second block is ever mapped.
        let good = heap.tuple(&[make_small(7), make_small(8)]).unwrap();
        let copied = store.copy(good).expect("a tuple is copyable");
        assert_eq!(store.words(), 3);
        assert_eq!(store.blocks(), 1, "the rolled-back run was reused");
        assert_eq!(store.free(copied), 3);
        assert_eq!(store.words(), 0);
    }

    /// `relinquish` hands an object's storage to the caller: the word count drops
    /// (it is no longer table memory) but the object stays valid, and its words are
    /// **never** handed out again — the difference from `free`.
    #[test]
    fn relinquish_checks_out_an_object_without_reusing_its_words() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(32);
        let source = heap.tuple(&[make_small(1), make_small(2)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(source).expect("a tuple is copyable");
        assert_eq!(store.words(), 3);

        // Checking out the object drops it from the live count …
        assert_eq!(store.relinquish(copied), 3);
        assert_eq!(store.words(), 0);

        // … but does not free it: a second relinquish / a free are no-ops …
        assert_eq!(store.relinquish(copied), 0);
        assert_eq!(store.free(copied), 0);

        // … and the object itself is untouched.
        assert!(eq(copied, source, &atoms));

        // The checked-out run is not reusable: a later same-size copy takes fresh
        // words, so the object the caller holds can never be overwritten.
        let other = heap.tuple(&[make_small(3), make_small(4)]).unwrap();
        let second = store.copy(other).expect("a tuple is copyable");
        assert_ne!(second, copied, "checked-out words are never handed out again");
        assert!(
            eq(copied, source, &atoms),
            "the checked-out object survived the later copy"
        );
        assert_eq!(store.words(), 3, "only the new copy is live table storage");
    }

    /// The **global empty tuple** — the one literal whose liveness this store can
    /// prove (`beam_term::tuple::tuple0` backs it with a leaked, never-reclaimed
    /// word) — is shared rather than duplicated, exactly as the C does for
    /// `ERTS_GLOBAL_LIT_EMPTY_TUPLE`. A copy that would otherwise cost
    /// `header + 2 elements + the literal's own header` costs only the outer tuple.
    #[test]
    fn the_global_empty_tuple_literal_is_shared_not_copied() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(32);
        // `{1, {}}`: the inner `{}` is the shared global literal.
        let with_literal = heap.tuple(&[make_small(1), tuple0()]).unwrap();
        // `{1, 2}`: the same shape with an immediate element, for a like-for-like
        // word count.
        let with_immediate = heap.tuple(&[make_small(1), make_small(2)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(with_literal).expect("a tuple is copyable");
        assert_eq!(
            store.words(),
            3,
            "only the outer tuple is copied — the nested literal contributes no words"
        );
        // The element is the very same literal word, not a heap copy of it.
        // SAFETY: `copied` is a live 2-element tuple just written by `copy`.
        assert_eq!(unsafe { tuple_element(copied, 1) }, tuple0());
        assert!(eq(copied, with_literal, &atoms), "the copy is structurally equal");
        assert_eq!(store.free(copied), 3);

        // A literal root is shared outright: it owns no store storage at all.
        let root = store.copy(tuple0()).expect("a literal root is copyable");
        assert_eq!(root, tuple0());
        assert_eq!(store.words(), 0, "a shared literal root costs no words");
        assert_eq!(store.free(root), 0, "and has nothing to reclaim");

        // A `{}`-free term of the same arity copies to the same size, which is what
        // makes the `3` above meaningful (there is no hidden extra allocation).
        let copied2 = store.copy(with_immediate).expect("a tuple is copyable");
        assert_eq!(store.words(), 3);
        assert_eq!(store.free(copied2), 3);
    }

    /// A source object of a *varying* shape, so one churn cycle exercises several
    /// free-run size buckets at once (an even index yields a tuple of a varying
    /// arity, an odd index a proper list of a varying length).
    fn churn_source(heap: &mut Heap, i: i64) -> Eterm {
        if i % 2 == 0 {
            let elements: Vec<Eterm> = (0..(i % 4) + 2)
                .map(|j| make_small(i * 100 + j))
                .collect();
            heap.tuple(&elements).expect("heap room for the source tuple")
        } else {
            let mut acc = NIL;
            for j in 0..(i % 5) + 2 {
                acc = heap
                    .cons(make_small(i * 100 + j), acc)
                    .expect("heap room for the source cons");
            }
            acc
        }
    }

    /// The **real-memory** proof of per-object reclamation: a long `copy` + `free`
    /// churn loop must reach a stable **block chain**.
    ///
    /// [`TermStore::blocks`]/[`TermStore::memory`] report the allocation actually
    /// held from the allocator (block *capacities*, which never shrink), so a store
    /// that failed to reuse a reclaimed run would map a fresh block every cycle and
    /// the figure would grow linearly with the cycle count — while
    /// [`TermStore::words`] alone would still return to zero, because it counts live
    /// words, not held memory. This is the test that a `words()`-only assertion
    /// cannot substitute for.
    #[test]
    fn copy_free_churn_reaches_a_stable_block_chain() {
        const NEAR_CYCLES: usize = 10;
        const FAR_CYCLES: usize = 1000;
        const OBJECTS: i64 = 64;

        let mut heap = Heap::new(4096);
        let sources: Vec<Eterm> = (0..OBJECTS).map(|i| churn_source(&mut heap, i)).collect();

        let mut store = test_store();
        let cycle = |store: &mut TermStore| {
            let copies: Vec<Eterm> = sources
                .iter()
                .map(|&source| store.copy(source).expect("every churn object is copyable"))
                .collect();
            for copy in copies {
                assert!(
                    store.free(copy) > 0,
                    "every churn object owns store storage"
                );
            }
            assert_eq!(store.words(), 0, "the cycle reclaims all of its storage");
        };

        for _ in 0..NEAR_CYCLES {
            cycle(&mut store);
        }
        let near_blocks = store.blocks();
        let near_memory = store.memory();
        assert!(near_blocks > 1, "the churn must span more than one block");
        assert!(near_memory > 0);

        for _ in 0..FAR_CYCLES {
            cycle(&mut store);
        }
        assert_eq!(
            store.blocks(),
            near_blocks,
            "a long churn must not map new blocks ({} cycles after {NEAR_CYCLES})",
            NEAR_CYCLES + FAR_CYCLES
        );
        assert_eq!(
            store.memory(),
            near_memory,
            "the held block memory must stabilize, not grow with the cycle count"
        );
        assert_eq!(store.words(), 0);
    }

    // --- the boxed kinds the oracle's `ets:insert/2` accepts -------------------

    /// A **hash map** (a >32-key HAMT, not a flat map) copies node by node: every HAMT
    /// node, collision node and `[Key, Value]` leaf cell is reproduced into table
    /// storage, so the copy is structurally equal to the source *and* survives the
    /// source heap being clobbered.
    #[test]
    fn hash_map_copy_is_self_contained_and_equal() {
        let atoms = AtomTable::new();
        const N: i64 = 40;
        let capacity = map_words_capacity(N as usize);
        let mut heap = Heap::new(1024);
        let pairs: Vec<(Eterm, Eterm)> = (0..N)
            .map(|i| (make_small(i), make_small(i * 7 + 1)))
            .collect();
        // SAFETY: the heap has room for `capacity` words and outlives the map.
        let map = unsafe {
            let hp = heap.alloc_words(capacity).expect("heap room for a hash map");
            write_map(hp, &pairs, &atoms).expect("a hash map is writable")
        };
        assert!(
            !unsafe { is_flatmap(map) },
            "{N} pairs must produce a HAMT, not a flat map"
        );

        let mut store = test_store();
        let copied = store.copy(map).expect("a hash map is copyable");
        assert_ne!(boxed_val(copied), boxed_val(map), "must be copied");
        assert!(eq(map, copied, &atoms), "structural equality");
        assert_eq!(map_size(copied, &atoms).expect("a live map"), N as usize);
        assert!(store.words() > 0, "the HAMT owns store storage");

        // Clobber the whole source-map region, then read the copy back: a copy that
        // still aliased a node or a leaf cell would now read different values.
        heap.reset();
        let _clobber = heap
            .tuple(&vec![make_small(-1); capacity - 1])
            .expect("heap room for the clobber tuple");
        let mut want = pairs.clone();
        want.sort_by(|a, b| cmp(a.0, b.0, &atoms));
        assert_eq!(
            map_pairs(copied, &atoms).expect("a live map"),
            want,
            "the copy must be self-contained"
        );

        assert!(store.free(copied) > 0, "the free reclaims every run");
        assert_eq!(store.words(), 0);
    }

    /// Builds the head node of a map whose single slot is a **collision node** — the
    /// `[make_arityval(n)][n leaf cells]` shape `beam_term::map` writes for >= 2 keys
    /// sharing a full 64-bit hash. `write_map` cannot produce it deterministically
    /// (finding such a key pair needs a hash search), so it is built by hand here:
    /// `[make_map_header(HEAD_BITMAP, 1, 1)][size = n][slot0 = coll]`, three words with
    /// one occupancy bit.
    ///
    /// # Safety
    ///
    /// `heap` must have room for `3 + n + 2n` words and must outlive the term.
    unsafe fn build_map_collision_node(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let n = pairs.len();
        // SAFETY: `alloc_tuple(n)` hands over the collision tuple's `n + 1` words, then
        // `2n` cons-cell words and the head node's 3 words are reserved. Every word is
        // written before the term escapes.
        unsafe {
            let coll = heap.alloc_tuple(n).expect("heap room for the collision node");
            for (i, (key, value)) in pairs.iter().enumerate() {
                let cell = heap
                    .alloc_words(2)
                    .expect("heap room for a collision leaf cell");
                *cell.add(CAR_OFFSET) = *key;
                *cell.add(CDR_OFFSET) = *value;
                *coll.add(1 + i) = make_list(cell as *const Eterm);
            }
            let head = heap.alloc_words(3).expect("heap room for the head node");
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1);
            *head.add(1) = Eterm::from_raw(n);
            *head.add(2) = make_boxed(coll as *const Eterm);
            make_boxed(head as *const Eterm)
        }
    }

    /// A HAMT whose slot is a **collision node** is copied recursively: the head node,
    /// the collision tuple and every leaf cell are deep-copied into table storage, so
    /// the copy is structurally equal and independent of the source.
    #[test]
    fn hash_map_collision_node_copy_is_recursive() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("alpha");
        let b = atoms.intern("beta");
        let mut heap = Heap::new(32);
        let pairs = [(make_atom(a), make_small(1)), (make_atom(b), make_small(2))];
        // SAFETY: the heap has room (3 + 2 + 4 words) and outlives the map.
        let map = unsafe { build_map_collision_node(&mut heap, &pairs) };

        let mut store = test_store();
        let copied = store.copy(map).expect("a collision-node map is copyable");
        assert_ne!(boxed_val(copied), boxed_val(map));
        assert!(eq(map, copied, &atoms), "structural equality");
        assert_eq!(map_size(copied, &atoms).expect("a live map"), 2);

        // The collision tuple is a distinct, deep-copied object, and so are its cells.
        // SAFETY: `copied` and `map` are live 3-word head nodes; the slots and cells are
        // live terms (the map's `size` word at word 1 is not a term).
        unsafe {
            let src_slot = *boxed_val(map).add(2);
            let dst_slot = *boxed_val(copied).add(2);
            assert!(is_boxed_term(dst_slot));
            assert_ne!(
                boxed_val(src_slot),
                boxed_val(dst_slot),
                "the collision node must be deep-copied"
            );
            assert_eq!(tuple_size(dst_slot), 2);
            let src_cell = tuple_element(src_slot, 0);
            let dst_cell = tuple_element(dst_slot, 0);
            assert_ne!(
                list_val(src_cell),
                list_val(dst_cell),
                "the leaf cells must be deep-copied"
            );
            assert_eq!(car(dst_cell), make_atom(a));
            assert_eq!(cdr(dst_cell), make_small(1));
        }

        let mut want: Vec<(Eterm, Eterm)> = pairs.to_vec();
        want.sort_by(|x, y| cmp(x.0, y.0, &atoms));
        assert_eq!(map_pairs(copied, &atoms).expect("a live map"), want);

        assert!(store.free(copied) > 0);
        assert_eq!(store.words(), 0);
    }

    /// Every fun kind (`Local`/`External`/`Imported`) copies with its header and
    /// descriptor words verbatim — only the `num_free` environment is deep-copied.
    #[test]
    fn every_fun_kind_copies_with_its_environment_deep_copied() {
        let mut atoms = AtomTable::new();
        let foo = atoms.intern("foo");
        let bar = atoms.intern("bar");
        let mut heap = Heap::new(64);
        // A boxed environment element for the local fun, so the deep-copy of the
        // environment is genuinely exercised.
        // SAFETY: the heap has room for the float and outlives every fun.
        let env_elem = unsafe { build_float(&mut heap, 2.5) };

        // SAFETY: each `alloc_words(fun_words(kind, num_free))` reserves exactly the
        // block the corresponding writer fills, and the heap outlives every fun.
        let (local, external, imported) = unsafe {
            let env = [make_small(7), env_elem];
            let hp = heap
                .alloc_words(fun_words(FunKind::Local, env.len()))
                .expect("heap room for a local fun");
            let local = write_fun_local(hp, 2, 0xDEAD_BEEF_0000_0001, 3, &env)
                .expect("a local fun is writable");
            let hp = heap
                .alloc_words(fun_words(FunKind::External, 0))
                .expect("heap room for an external fun");
            let external =
                write_fun_external(hp, 3, foo, bar).expect("an external fun is writable");
            let env = [make_small(9)];
            let hp = heap
                .alloc_words(fun_words(FunKind::Imported, env.len()))
                .expect("heap room for an imported fun");
            let imported = write_fun_imported(
                hp,
                4,
                foo,
                &[0xAB; 16],
                5,
                6,
                7,
                FunPid {
                    node: bar,
                    number: 8,
                    serial: 9,
                    creation: 10,
                },
                &env,
            )
            .expect("an imported fun is writable");
            (local, external, imported)
        };

        let mut store = test_store();
        for (name, source) in [
            ("local", local),
            ("external", external),
            ("imported", imported),
        ] {
            let copied = store.copy(source).expect("a fun is copyable");
            assert_ne!(
                boxed_val(copied),
                boxed_val(source),
                "{name}: must be deep-copied"
            );
            assert!(eq(source, copied, &atoms), "{name}: structural equality");
            // SAFETY: both are live fun terms.
            assert_eq!(
                unsafe { fun_kind_of_header(*boxed_val(copied)) },
                unsafe { fun_kind_of_header(*boxed_val(source)) },
                "{name}: the kind survives"
            );
            assert!(store.free(copied) > 0, "{name}: holds store storage");
        }

        // The local fun's boxed environment element is a distinct, deep-copied thing.
        let copied_local = store.copy(local).expect("a local fun is copyable");
        // SAFETY: both are live local funs with the same `num_free`; the boxed elements
        // are live floats.
        unsafe {
            let env_off = 1 + fun_desc_words(FunKind::Local);
            let src_elem = *boxed_val(local).add(env_off + 1);
            let dst_elem = *boxed_val(copied_local).add(env_off + 1);
            assert!(is_float(src_elem) && is_float(dst_elem));
            assert_ne!(
                boxed_val(src_elem),
                boxed_val(dst_elem),
                "the environment must be deep-copied"
            );
            assert!(eq(src_elem, dst_elem, &atoms));
        }
        assert!(store.free(copied_local) > 0);
        assert_eq!(store.words(), 0);
    }

    /// External pid/port/reference blocks are copied with their raw `u32`/`u64` payload
    /// verbatim, while the `node` term slot is deep-copied when it is not an immediate.
    #[test]
    fn external_pid_port_and_ref_copy_their_node_slot_deeply() {
        let mut atoms = AtomTable::new();
        let node_atom = atoms.intern("n1@host");
        let mut heap = Heap::new(64);
        // A **boxed** node term for the pid, so the deep-copy of the node slot is
        // exercised: a boxed node must never be left pointing at the source.
        // SAFETY: the heap has room for the float and outlives the pid.
        let boxed_node = unsafe { build_float(&mut heap, 6.25) };
        let ids = [0x1111_1111u32, 0x2222_2222, 0x3333_3333];

        // SAFETY: each `alloc_words(w)` reserves exactly the block the writer fills, and
        // the heap outlives every term.
        let (pid, port, ext_ref) = unsafe {
            let hp = heap
                .alloc_words(external_pid_size_words())
                .expect("heap room for an external pid");
            let pid = write_external_pid(hp, boxed_node, 1000, 7, 5);
            let hp = heap
                .alloc_words(external_port_size_words())
                .expect("heap room for an external port");
            let port = write_external_port(hp, make_atom(node_atom), 0x1_0000_0007, 5);
            let hp = heap
                .alloc_words(external_ref_size_words(ids.len()))
                .expect("heap room for an external ref");
            let ext_ref = write_external_ref(hp, make_atom(node_atom), &ids, 5);
            (pid, port, ext_ref)
        };

        let mut store = test_store();
        for (name, source) in [("pid", pid), ("port", port), ("ref", ext_ref)] {
            let copied = store.copy(source).expect("an external thing is copyable");
            assert_ne!(
                boxed_val(copied),
                boxed_val(source),
                "{name}: must be deep-copied"
            );
            assert!(eq(source, copied, &atoms), "{name}: structural equality");
            assert!(store.free(copied) > 0, "{name}: owns store storage");
        }
        assert_eq!(store.words(), 0);

        // The pid's boxed node landed in table storage; its scalars are verbatim.
        let copied_pid = store.copy(pid).expect("an external pid is copyable");
        // SAFETY: both pids are live; the copied node is a live store-owned float.
        unsafe {
            let src_node = *boxed_val(pid).add(1);
            let dst_node = *boxed_val(copied_pid).add(1);
            assert!(is_float(src_node) && is_float(dst_node));
            assert_ne!(
                boxed_val(src_node),
                boxed_val(dst_node),
                "the node slot must be deep-copied"
            );
            assert!(eq(src_node, dst_node, &atoms));
            let (_, number, serial, creation) = external_pid_parts(copied_pid);
            assert_eq!((number, serial, creation), (1000, 7, 5));
        }
        assert!(store.free(copied_pid) > 0);
        assert_eq!(store.words(), 0);
    }

    /// An off-heap **refc binary** is re-materialised as a **self-contained on-heap
    /// binary**: [`TermStore::copy_sub_bits`] gathers the payload bytes and writes a
    /// fresh `ErlHeapBits`, so the copy neither aliases the off-heap payload nor depends
    /// on it. Dropping the payload after the copy must not disturb the copy.
    #[test]
    fn refc_binary_copy_is_a_self_contained_on_heap_binary() {
        let atoms = AtomTable::new();
        let bytes = [0xA3u8, 0x5C, 0xF0, 0x0F, 0x12, 0x34, 0x56, 0x78];
        let mut heap = Heap::new(32);
        // SAFETY: the heap has room for the 8-word refc pair and outlives the term.
        let (refc, payload) = unsafe { build_refc_binary(&mut heap, &bytes) };
        assert!(unsafe { is_bitstring(refc) });

        let mut store = test_store();
        let copied = store.copy(refc).expect("a refc binary is copyable");
        assert_ne!(boxed_val(copied), boxed_val(refc), "must be copied");
        assert!(eq(refc, copied, &atoms), "the copy holds the same bytes");
        // The copy is an on-heap bitstring, not another off-heap reference.
        // SAFETY: `copied` is a live boxed bitstring.
        unsafe {
            assert_eq!(
                header_subtag(*boxed_val(copied)),
                HEAP_BITS_SUBTAG as u32,
                "the copy must be an on-heap bitstring"
            );
            assert!(is_binary(copied));
            assert_eq!(bitstring_size(copied), bytes.len() * 8);
        }

        // Drop the off-heap payload: `refc` now dangles, but the copy must not.
        drop(payload);
        // SAFETY: `copied` is store-owned, on-heap and self-contained.
        unsafe {
            let (base, offset, size) = bitstring_parts(copied);
            assert_eq!((offset, size), (0, bytes.len() * 8));
            assert_eq!(core::slice::from_raw_parts(base, bytes.len()), &bytes[..]);
        }

        assert!(store.free(copied) > 0);
        assert_eq!(store.words(), 0);
    }

    /// Pins the independent window expectation used by
    /// [`sub_bitstring_copy_is_self_contained`] against a hand computation, so that test
    /// cannot pass by the expectation and the production gatherer being wrong together.
    #[test]
    fn the_window_expectation_matches_a_hand_computation() {
        // Bits 1..9 of `1010_1010 1100_1100` are `0101_0101`.
        assert_eq!(window_bytes(&[0xAA, 0xCC], 1, 8), vec![0x55]);
        // Bits 3..13 of `1010_0011 0101_1100` are `0001_1010 11` -> `1A C0`.
        assert_eq!(window_bytes(&[0xA3, 0x5C], 3, 10), vec![0x1A, 0xC0]);
    }

    /// A **non-byte-aligned** sub-bitstring (`start = 3`, `size = 13`) copies as a
    /// self-contained on-heap bitstring whose bits equal the source window — the
    /// bit-by-bit gather path, not the aligned slice copy. Dropping the payload after
    /// the copy must not disturb it.
    #[test]
    fn sub_bitstring_copy_is_self_contained() {
        let atoms = AtomTable::new();
        let payload_bytes = [0xA3u8, 0x5C];
        let expected = window_bytes(&payload_bytes, 3, 13);
        assert_eq!(expected.len(), 2, "13 bits is two (partial) bytes");

        let mut heap = Heap::new(32);
        // SAFETY: the heap has room for the 5-word `ErlSubBits` and outlives the term.
        let (sub_bits, payload) = unsafe { build_non_aligned_sub_bits(&mut heap, &payload_bytes) };
        assert!(unsafe { is_bitstring(sub_bits) });
        assert!(
            !unsafe { is_binary(sub_bits) },
            "13 bits is not a whole number of bytes"
        );

        let mut store = test_store();
        let copied = store.copy(sub_bits).expect("a sub-bitstring is copyable");
        assert_ne!(boxed_val(copied), boxed_val(sub_bits), "must be copied");
        assert!(eq(sub_bits, copied, &atoms), "the copy holds the same bits");
        // SAFETY: `copied` is a live boxed bitstring.
        unsafe {
            assert_eq!(
                header_subtag(*boxed_val(copied)),
                HEAP_BITS_SUBTAG as u32,
                "the copy must be an on-heap bitstring"
            );
            assert_eq!(bitstring_size(copied), 13);
        }

        // Drop the payload: `sub_bits` now dangles; the copy must not.
        drop(payload);
        // SAFETY: `copied` is store-owned, on-heap and self-contained.
        unsafe {
            let (base, offset, size) = bitstring_parts(copied);
            assert_eq!((offset, size), (0, 13));
            assert_eq!(
                core::slice::from_raw_parts(base, 2),
                &expected[..],
                "the copy's bits are the source window"
            );
        }

        // A byte-equivalent on-heap bitstring built independently compares equal.
        let mut heap2 = Heap::new(8);
        // SAFETY: `heap_bits_size_words(13)` words are reserved for the reference term.
        let reference = unsafe {
            let hp = heap2
                .alloc_words(heap_bits_size_words(13))
                .expect("heap room for the reference bitstring");
            write_heap_bits(hp, &expected, 13).expect("a bitstring is writable")
        };
        assert!(eq(copied, reference, &atoms));

        assert!(store.free(copied) > 0);
        assert_eq!(store.words(), 0);
    }

    /// Per-object accounting over the two newly-supported multi-run kinds: a hash map
    /// and a fun both reclaim exactly the words they consumed, and a **later** copy
    /// reuses those runs without disturbing a still-live term.
    #[test]
    fn hash_map_and_fun_reclamation_keeps_the_accounting_stable() {
        let atoms = AtomTable::new();
        const N: i64 = 40;
        let mut heap = Heap::new(1024);
        let pairs: Vec<(Eterm, Eterm)> = (0..N)
            .map(|i| (make_small(i), make_small(i + 100)))
            .collect();
        // SAFETY: the heap has room for the map (<= `map_words_capacity(N)`) and the fun.
        let (map, fun) = unsafe {
            let hp = heap
                .alloc_words(map_words_capacity(pairs.len()))
                .expect("heap room for a hash map");
            let map = write_map(hp, &pairs, &atoms).expect("a hash map is writable");
            let env = [make_small(5)];
            let hp = heap
                .alloc_words(fun_words(FunKind::Local, env.len()))
                .expect("heap room for a fun");
            let fun =
                write_fun_local(hp, 1, 0x1234, 2, &env).expect("a local fun is writable");
            (map, fun)
        };

        let mut store = test_store();
        let map_copy = store.copy(map).expect("a hash map is copyable");
        let fun_copy = store.copy(fun).expect("a fun is copyable");
        let live = store.words();
        let blocks = store.blocks();
        assert!(live > 0, "both roots own store storage");
        assert!(blocks > 0);

        // Free only the map, then re-copy it: the still-live fun must be undisturbed and
        // the recopy must reuse the freed runs (no new block, no new memory).
        let map_words = store.free(map_copy);
        assert!(map_words > 0);
        assert_eq!(store.words(), live - map_words);
        let map_copy2 = store.copy(map).expect("a hash map is copyable");
        assert!(eq(map, map_copy2, &atoms), "the recopy is structurally equal");
        assert!(eq(fun, fun_copy, &atoms), "the live fun survived the map churn");
        assert_eq!(store.blocks(), blocks, "reuse mapped no new block");
        assert_eq!(store.words(), live, "the recopy cost exactly the freed words");

        // Free both roots: the live count returns to zero; the blocks stay mapped.
        assert_eq!(store.free(map_copy2), map_words, "the map's runs");
        assert_eq!(store.free(fun_copy), live - map_words, "the fun's runs");
        assert_eq!(store.words(), 0);
        assert_eq!(store.blocks(), blocks, "reclamation never unmaps a block");
        assert!(store.memory() > 0, "the freed words' storage is still held");
    }