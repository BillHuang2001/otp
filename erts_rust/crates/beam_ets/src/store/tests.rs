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

    /// Builds a sub-bitstring thing (`[header, bit_offset, bit_size, off_heap_bin,
    /// pad]`) — a kind that aliases storage outside the term graph and is therefore
    /// *not* in the copied subset.
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

    /// The `badarg` path for term kinds outside the copied subset.
    #[test]
    fn sub_bitstring_is_rejected_with_badarg() {
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