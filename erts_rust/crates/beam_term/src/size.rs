//! Object word size (`size_object` / `erts_size_object`).
//!
//! Ground truth: `size_object_x(obj, NULL)` in `erts/emulator/beam/copy.c`,
//! exported as `#define size_object(Term) size_object_x(Term,NULL)` in
//! `erts/emulator/beam/global.h`. It returns the **flat** number of heap words
//! the term graph occupies — the count `copy_struct` uses to size a destination
//! heap, and the amount `beam_gc`/`beam_message` must reserve.
//!
//! The convention (confirmed against `size_object_x`):
//!
//! * an **immediate** (small int, atom, `nil`, internal pid/port, catch) → `0`
//!   words;
//! * a **cons cell** → `2` words (`[CAR, CDR]`, header-less), **plus** the sizes
//!   of its car and cdr;
//! * a **boxed object** → `arity + 1` words (header + payload), **plus** the
//!   sizes of any nested terms it contains.
//!
//! so `size_object(42) == 0`, `size_object([1]) == 2`,
//! `size_object({1,2,3}) == 4`, and `size_object([1,2,3]) == 6`.
//!
//! # Internal layouts
//!
//! Like the C, every navigable interior is walked:
//!
//! * a **hash-map (HAMT) node** — head-array, head-bitmap and inner node-bitmap
//!   alike — contributes `1 + popcount(MAP_HEADER_VAL) + header_arity` words and
//!   every non-immediate slot is traversed, so the whole trie (leaves, sub-nodes
//!   and collision tuples) is counted (`copy.c:192-214`);
//! * an **`ErlSubBits`** (`SUB_BITS_SUBTAG`) whose `orig` is a `BinRef` is a full
//!   refc binary and contributes the whole on-heap `[BinRef(3)][ErlSubBits(5)]`
//!   pair ([`ERL_REFC_BITS_SIZE`], `8` words); a match context over an on-heap
//!   bitstring instead contributes the size of the *copied* window
//!   (`copy.c:219-242`).
//!
//! # Known Issues
//!
//! * **Native records** (`RECORD_SUBTAG`) are treated like tuples (their
//!   field-table layout is not yet modelled).

use crate::bitstring::{heap_bits_size_words, sub_bits_parts};
use crate::boxed::{
    boxed_val, header_arity, is_boxed, is_flatmap_header, map_header_val, thing_arityval,
    ERL_FUN_SIZE, ERL_SUB_BITS_SIZE, HEADER_BIN_REF,
};
use crate::eterm::Eterm;
use crate::immediate::is_immediate;
use crate::list::{car, cdr, is_list};
use crate::refc::ERL_REFC_BITS_SIZE;
use crate::tag::{
    ARITYVAL_SUBTAG, FUN_SUBTAG, MAP_SUBTAG, RECORD_SUBTAG, SUB_BITS_SUBTAG, TAG_HEADER_MASK,
};

/// `FUN_HEADER_ENV_SIZE_OFFS` (`erl_fun.h`): the bit offset of a fun header's
/// environment-size (`fun_num_free`) field.
const FUN_HEADER_ENV_SIZE_OFFS: u32 = 16;

/// The flat number of heap words the term graph `a` occupies.
///
/// Reproduces `size_object(a)` == `size_object_x(a, NULL)`. See the module docs
/// for the convention and the documented approximations.
pub fn size_object(a: Eterm) -> usize {
    let mut total = 0usize;
    // An explicit worklist linearises lists and avoids native-stack recursion.
    let mut work: Vec<Eterm> = Vec::new();
    let mut current = a;

    loop {
        if is_immediate(current) {
            // Immediates occupy no heap words.
        } else if is_list(current) {
            // A cons cell is a header-less 2-word object.
            total += 2;
            // SAFETY: `current` is a live non-`NIL` cons cell.
            let (head, tail) = unsafe { (car(current), cdr(current)) };
            if !is_immediate(head) {
                work.push(head);
            }
            // Linearise the list: continue with the tail.
            current = tail;
            continue;
        } else {
            // SAFETY: `current` is a live boxed term; its header word is valid.
            let p = boxed_val(current);
            let header = unsafe { *p };
            match header.raw() & TAG_HEADER_MASK {
                ARITYVAL_SUBTAG | RECORD_SUBTAG => {
                    let arity = header_arity(header);
                    // The zero-arity tuple is the shared global literal: 0 words.
                    if arity > 0 {
                        total += arity + 1;
                        for i in 0..arity {
                            // SAFETY: element `i` is within the payload.
                            let elem = unsafe { *p.add(1 + i) };
                            if !is_immediate(elem) {
                                work.push(elem);
                            }
                        }
                    }
                }
                FUN_SUBTAG => {
                    let num_free = ((header.raw() >> FUN_HEADER_ENV_SIZE_OFFS) & 0xFF) as usize;
                    // ERL_FUN_SIZE does not include the environment.
                    total += ERL_FUN_SIZE + num_free;
                    for i in 0..num_free {
                        // SAFETY: environment word `i` is within the payload.
                        let elem = unsafe { *p.add(ERL_FUN_SIZE + i) };
                        if !is_immediate(elem) {
                            work.push(elem);
                        }
                    }
                }
                MAP_SUBTAG => {
                    if is_flatmap_header(header) {
                        // flatmap_t = [thing_word, size, keys(tuple), V0..V(size-1)].
                        let size = unsafe { *p.add(1) }.raw();
                        total += size + 3;
                        for i in 0..=size {
                            // SAFETY: word `2 + i` is within the payload.
                            let elem = unsafe { *p.add(2 + i) };
                            if !is_immediate(elem) {
                                work.push(elem);
                            }
                        }
                    } else {
                        // Every other map header is a HAMT node: a head-array, a
                        // head-bitmap or an inner node-bitmap. `header_arity(header)`
                        // is `1` for the two head kinds (their extra `size` word) and
                        // `0` for an inner node, so the slots start at
                        // `1 + header_arity`. Mirrors `size_object_x`
                        // (`copy.c:192-214`): `sum += 1 + popcount(MAP_HEADER_VAL) +
                        // header_arity`, then pushes every slot so the whole trie
                        // (leaves, sub-nodes and collision tuples) is reached.
                        let arity = header_arity(header);
                        let nslots = map_header_val(header).count_ones() as usize;
                        total += 1 + nslots + arity;
                        for i in 0..nslots {
                            // SAFETY: slot `i` is within the node's payload.
                            let slot = unsafe { *p.add(1 + arity + i) };
                            if !is_immediate(slot) {
                                work.push(slot);
                            }
                        }
                    }
                }
                SUB_BITS_SUBTAG => {
                    // `[HEADER_SUB_BITS, base_flags, start, end, orig]` (`copy.c:219-242`).
                    let (_base_flags, start, end, orig) = unsafe { sub_bits_parts(current) };
                    if is_boxed(orig) {
                        // SAFETY: `orig` is a boxed term; the C reads
                        // `*boxed_val(sb->orig)` unconditionally (`copy.c:222`).
                        let underlying = unsafe { *boxed_val(orig) };
                        if underlying == HEADER_BIN_REF {
                            // A full refc binary: the on-heap representation is the
                            // `[BinRef(3)][ErlSubBits(5)]` pair whose root is this
                            // `ErlSubBits`; the C counts the whole pair in one go
                            // (the `BinRef`'s own `val`/`next` are off-heap scalar
                            // pointers contributing no heap words).
                            total += ERL_REFC_BITS_SIZE;
                        } else {
                            // A match context over an on-heap bitstring: the copy
                            // materialises a fresh heap-bits block for the window
                            // (which is why the underlying thing is not traversed).
                            total += heap_bits_size_words(end.raw().saturating_sub(start.raw()));
                        }
                    } else {
                        // Not a term the C accepts: `boxed_val(sb->orig)` would
                        // dereference a bogus pointer there. A standalone/malformed
                        // `ErlSubBits` reports its own thing size instead.
                        total += ERL_SUB_BITS_SIZE;
                    }
                }
                _ => {
                    // Float, bignum, ref, bin ref, heap bits, external
                    // pid/port/ref: header + payload. None of these has navigable
                    // subterms.
                    total += thing_arityval(header) + 1;
                }
            }
        }

        match work.pop() {
            Some(next) => current = next,
            None => return total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstring::{write_heap_bits, write_sub_bits};
    use crate::boxed::{
        arityval, is_arity_value, is_map_header, make_arityval, make_boxed, make_header,
        map_header_type, HEADER_FLONUM,
    };
    use crate::heap::Heap;
    use crate::immediate::{
        make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data,
        make_small, NIL,
    };
    use crate::map::{make_map_header, map_words_capacity, write_map};
    use crate::refc::write_refc_binary;
    use crate::tag::{
        HEAP_BITS_SUBTAG, MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
        MAP_HEADER_TAG_HAMT_NODE_BITMAP,
    };
    use crate::tuple::tuple0;
    use crate::AtomTable;

    #[test]
    fn immediates_have_zero_size() {
        let mut table = crate::AtomTable::new();
        let atom = make_atom(table.intern("some_atom"));
        for t in [
            make_small(0),
            make_small(1234),
            atom,
            NIL,
            make_internal_pid(make_pid_data(0, 5)),
            make_internal_port(make_port_data(5)),
        ] {
            assert_eq!(size_object(t), 0, "immediate {t:?} should be 0 words");
        }
    }

    #[test]
    fn cons_cells_are_two_words_each() {
        let mut heap = Heap::new(16);
        let one = heap.cons(make_small(1), NIL).unwrap();
        assert_eq!(size_object(one), 2);
        let tail3 = heap.cons(make_small(3), NIL).unwrap();
        let tail23 = heap.cons(make_small(2), tail3).unwrap();
        let three = heap.cons(make_small(1), tail23).unwrap();
        assert_eq!(size_object(three), 6);
    }

    #[test]
    fn tuples_are_header_plus_payload() {
        let mut heap = Heap::new(16);
        let t3 = heap.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
        assert_eq!(size_object(t3), 4);
        // Nested: outer arity 2 (=> 3 words) + inner arity 2 (=> 3 words).
        let inner = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer = heap.tuple(&[inner, make_small(9)]).unwrap();
        assert_eq!(size_object(outer), 6);
    }

    #[test]
    fn zero_arity_tuple_literal_is_zero_words() {
        assert_eq!(size_object(tuple0()), 0);
    }

    #[test]
    fn boxed_leaf_objects_are_header_plus_payload() {
        // A 64-bit flonum is a 1-word thing => 2 words total.
        let float_words: [Eterm; 2] = [HEADER_FLONUM, Eterm::from_raw(0)];
        let float = make_boxed(float_words.as_ptr());
        assert_eq!(size_object(float), 2);

        // A 16-bit heap binary: [thing_word, size, 1 data word] => 3 words.
        let mut heap = Heap::new(4);
        // SAFETY: the heap owns these words.
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16);
            *p.add(2) = Eterm::from_raw(0x1234);
        }
        let bin = make_boxed(p);
        assert_eq!(size_object(bin), 3);
    }

    #[test]
    fn flatmap_counts_header_size_keys_and_values() {
        // SAFETY: the heap owns every word built here.
        let mut heap = Heap::new(16);
        let keys = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let p = unsafe { heap.alloc_words(5).unwrap() };
        unsafe {
            *p = make_header(0, crate::tag::HAMT_SUBTAG_HEAD_FLATMAP);
            *p.add(1) = Eterm::from_raw(2); // flatmap size
            *p.add(2) = keys;
            *p.add(3) = make_small(7);
            *p.add(4) = make_small(8);
        }
        let map = make_boxed(p);
        // 2 + 3 = 5 words for the flatmap itself, plus the 3-word keys tuple.
        assert_eq!(size_object(map), 5 + 3);
    }

    #[test]
    fn size_is_additive_over_subterms() {
        let mut heap = Heap::new(32);
        let leaf = make_small(1);
        assert_eq!(size_object(leaf), 0);
        let t1 = heap.tuple(&[leaf]).unwrap();
        let l1 = heap.cons(t1, NIL).unwrap();
        // cons (2) + tuple {1} (2) + immediate (0)
        assert_eq!(size_object(l1), 4);
    }

    // -----------------------------------------------------------------------
    // Regression tests: a HAMT (hash map) trie and a refc binary must be counted
    // in full. Both used to be under-counted (only the head node resp. only the
    // on-heap `ErlSubBits` thing size).
    // -----------------------------------------------------------------------

    /// A map laid out into an owned buffer that outlives the returned term.
    struct BuiltMap {
        _buf: Vec<Eterm>,
        map: Eterm,
    }

    /// Lays `pairs` out with [`write_map`] into a buffer of exactly
    /// [`map_words_capacity`]`(pairs.len())` words.
    fn build_map(pairs: &[(Eterm, Eterm)]) -> BuiltMap {
        let atoms = AtomTable::new();
        let cap = map_words_capacity(pairs.len());
        let mut buf = vec![Eterm::from_raw(0xDEAD_BEEF); cap];
        // SAFETY: `buf` owns `cap` words and is kept alive by the returned value.
        let map = unsafe { write_map(buf.as_mut_ptr(), pairs, &atoms) }.unwrap();
        BuiltMap { _buf: buf, map }
    }

    /// `n` distinct integer keys; the values are immediates and add no words.
    fn int_pairs(n: i64) -> Vec<(Eterm, Eterm)> {
        (1..=n)
            .map(|k| (make_small(k), make_small(k * 7)))
            .collect()
    }

    /// A two-word `[Key, Value]` leaf cell allocated from `heap`.
    fn leaf(heap: &mut Heap, key: i64) -> Eterm {
        heap.cons(make_small(key), make_small(key * 10)).unwrap()
    }

    /// What an independent walk of a HAMT trie found.
    #[derive(Default)]
    struct HamtWalk {
        /// One per key.
        leaves: usize,
        /// The words of every node (header, `size` word and slots) and of every
        /// collision-node tuple.
        node_words: usize,
        inner_nodes: usize,
        collision_nodes: usize,
        head_array: bool,
        head_bitmap: bool,
    }

    impl HamtWalk {
        /// The whole map's words: its node words plus two per leaf cons cell.
        fn total(&self) -> usize {
            self.node_words + 2 * self.leaves
        }
    }

    /// Counts a HAMT trie node by node, deriving each node's size **by hand** from
    /// the layout table in [`crate::map`] (ground truth `erl_map.h:139-197`):
    /// head-array `2 + 16`, bitmap node `slots_offset + popcount(bitmap)`, collision
    /// node `1 + cells`. Deliberately a *different* formulation from
    /// `size_object`'s `1 + nslots + header_arity` worklist, so the two do not share
    /// a bug.
    fn walk_hamt(t: Eterm, is_head: bool, out: &mut HamtWalk) {
        let p = boxed_val(t);
        // SAFETY: `t` is a live node of the map being walked.
        let header = unsafe { *p };
        let (slots_off, nslots) = match map_header_type(header) as usize {
            MAP_HEADER_TAG_HAMT_HEAD_ARRAY => {
                assert!(is_head, "an array node can only be the head");
                out.head_array = true;
                (2, 16)
            }
            MAP_HEADER_TAG_HAMT_HEAD_BITMAP => {
                assert!(is_head, "a head-bitmap node can only be the head");
                out.head_bitmap = true;
                (2, map_header_val(header).count_ones() as usize)
            }
            MAP_HEADER_TAG_HAMT_NODE_BITMAP => {
                assert!(!is_head, "the head node is never an inner node");
                out.inner_nodes += 1;
                (1, map_header_val(header).count_ones() as usize)
            }
            other => panic!("not a HAMT node header (map tag {other})"),
        };
        out.node_words += slots_off + nslots;
        for i in 0..nslots {
            // SAFETY: slot `i` is within the node's payload.
            let slot = unsafe { *p.add(slots_off + i) };
            assert!(!is_immediate(slot), "a HAMT slot never holds an immediate");
            if is_list(slot) {
                out.leaves += 1;
            } else {
                // SAFETY: a non-leaf slot is a live boxed sub-node or collision tuple.
                let sub = unsafe { *boxed_val(slot) };
                if is_map_header(sub) {
                    walk_hamt(slot, false, out);
                } else {
                    assert!(is_arity_value(sub), "a collision node is a tuple");
                    let cells = arityval(sub);
                    out.collision_nodes += 1;
                    out.node_words += 1 + cells;
                    out.leaves += cells;
                }
            }
        }
    }

    /// The expected `size_object` of a hash map, computed by [`walk_hamt`].
    fn expected_hashmap_words(map: Eterm) -> HamtWalk {
        let mut out = HamtWalk::default();
        walk_hamt(map, true, &mut out);
        out
    }

    #[test]
    fn flatmap_sizes_at_the_hashmap_boundary_are_hand_computed() {
        // A non-empty flat map of `n` pairs is `2n + 4` words: the
        // `[arityval(n), keys…, head, n, keys-tuple, values…]` block (`4 + 2n`,
        // `thing::flatmap_size_words`) counted as its `n + 3`-word body plus the
        // `n + 1`-word keys tuple. The empty map is the special 3-word
        // `[head, 0, tuple0()]` block (the zero-arity tuple is a shared literal).
        for (n, expected) in [(0usize, 3usize), (1, 6), (2, 8), (31, 66), (32, 68)] {
            let b = build_map(&int_pairs(n as i64));
            assert!(is_flatmap_header(unsafe { *boxed_val(b.map) }));
            assert_eq!(size_object(b.map), expected, "flat map of {n} keys");
        }
        // 33 keys is the smallest hash map: its size follows the trie instead.
        let hash = build_map(&int_pairs(33));
        assert!(!is_flatmap_header(unsafe { *boxed_val(hash.map) }));
        let walk = expected_hashmap_words(hash.map);
        assert_eq!(size_object(hash.map), walk.total());
        assert_ne!(size_object(hash.map), 2 * 33 + 4);
    }

    #[test]
    fn hashmap_trie_is_counted_in_full() {
        for n in [33i64, 40, 100, 500] {
            let b = build_map(&int_pairs(n));
            assert!(
                !is_flatmap_header(unsafe { *boxed_val(b.map) }),
                "{n} keys must be a hash map"
            );
            let walk = expected_hashmap_words(b.map);
            assert!(
                walk.head_array || walk.head_bitmap,
                "the head is a HAMT node"
            );
            assert_eq!(walk.leaves, n as usize, "one leaf cell per key");
            assert_eq!(
                size_object(b.map),
                walk.total(),
                "every HAMT node and leaf of the {n}-key map must be counted"
            );
        }
    }

    #[test]
    fn hashmap_regression_is_no_longer_the_head_only_under_count() {
        let b = build_map(&int_pairs(500));
        let walk = expected_hashmap_words(b.map);
        assert!(
            walk.inner_nodes > 0,
            "a 500-key map must contain inner HAMT node-bitmap nodes"
        );
        assert_eq!(size_object(b.map), walk.total());
        // The figure the previous dispatch returned: the head node's own words
        // only (`1 + popcount(head bitmap) + head arity`, at most `18`), with no
        // descent into the trie at all.
        let head = unsafe { *boxed_val(b.map) };
        let old_head_only = 1 + map_header_val(head).count_ones() as usize + header_arity(head);
        assert!(old_head_only <= 18);
        assert!(
            size_object(b.map) > old_head_only,
            "the 500-key map is reported as {old_head_only} words (the old under-count)"
        );
    }

    #[test]
    fn every_hashmap_node_kind_is_counted() {
        // (a) Head bitmap holding two leaf slots:
        //     `[hdr][size][slot0][slot1]` = 4 node words + 2 * 2 leaf words = 8.
        let mut heap = Heap::new(32);
        let l0 = leaf(&mut heap, 1);
        let l1 = leaf(&mut heap, 2);
        // SAFETY: 4 words reserved for the head node.
        let head = unsafe { heap.alloc_words(4).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0011);
            *head.add(1) = Eterm::from_raw(2); // the head's key-count `size` word
            *head.add(2) = l0;
            *head.add(3) = l1;
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 8);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_bitmap && walk.inner_nodes == 0 && walk.collision_nodes == 0);
        assert_eq!(walk.total(), 8);

        // (b) Head array: `[hdr][size][16 slots]` = 18 node words + 16 leaves.
        let mut heap = Heap::new(64);
        let leaves: Vec<Eterm> = (0..16).map(|k| leaf(&mut heap, k)).collect();
        // SAFETY: 18 words reserved for the head-array node.
        let head = unsafe { heap.alloc_words(18).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff);
            *head.add(1) = Eterm::from_raw(16);
            for (i, cell) in leaves.iter().enumerate() {
                *head.add(2 + i) = *cell;
            }
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 18 + 16 * 2);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_array && walk.leaves == 16);
        assert_eq!(walk.total(), 18 + 16 * 2);

        // (c) Head bitmap -> inner node bitmap -> collision tuple of two cells:
        //     head 3 + inner 2 + tuple 3 + 2 * 2 leaf words = 12.
        let mut heap = Heap::new(32);
        let c0 = leaf(&mut heap, 1);
        let c1 = leaf(&mut heap, 2);
        // SAFETY: 3 words reserved for the collision-node tuple.
        let coll = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *coll = make_arityval(2);
            *coll.add(1) = c0;
            *coll.add(2) = c1;
        }
        // SAFETY: 2 words reserved for the inner node bitmap.
        let inner = unsafe { heap.alloc_words(2).unwrap() };
        unsafe {
            *inner = make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b0001);
            *inner.add(1) = make_boxed(coll);
        }
        // SAFETY: 3 words reserved for the head node.
        let head = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b1000);
            *head.add(1) = Eterm::from_raw(2);
            *head.add(2) = make_boxed(inner);
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 12);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_bitmap && walk.inner_nodes == 1 && walk.collision_nodes == 1);
        assert_eq!(walk.total(), 12);
    }

    #[test]
    fn refc_binary_counts_the_whole_bin_ref_pair() {
        // A refc binary — an `ErlSubBits` whose `orig` is a `BinRef` — is the 8-word
        // on-heap pair `[BinRef(3)][ErlSubBits(5)]`; `size_object_x` counts the whole
        // pair in one go (`copy.c:224-231`). The system ERTS agrees:
        // `erts_debug:flat_size(binary:copy(<<0>>, 100)) == 8`.
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut heap = Heap::new(16);
        // SAFETY: 8 words reserved for the pair; `payload` outlives the term.
        let term = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                payload.0.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.0.as_ptr(),
                0,
                64,
            )
        };
        assert_eq!(size_object(term), 8);
        // Reached through a composite term the pair is still counted exactly once.
        let cell = heap.cons(term, NIL).unwrap();
        assert_eq!(size_object(cell), 2 + 8);
    }

    #[test]
    fn sub_bits_match_context_counts_the_copied_heap_bits() {
        // A match context whose `orig` is an on-heap bitstring: the copy
        // materialises a fresh `ErlHeapBits` block for the window rather than the
        // match context itself (`copy.c:232-239`).
        let mut heap = Heap::new(64);
        // SAFETY: 3 words for a 64-bit (8-byte) heap bitstring.
        let base = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(64)).unwrap();
            write_heap_bits(hp, &[0xAA; 8], 64).unwrap()
        };
        assert_eq!(size_object(base), 3);
        // `heap_bits_size(bits) == 2 + ceil(bits / 64)`.
        for (bits, expected) in [(9usize, 3usize), (64, 3), (65, 4), (520, 11)] {
            // SAFETY: 5 words for the `ErlSubBits`; `base` is a live heap bitstring.
            let sub = unsafe {
                let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
                write_sub_bits(
                    hp,
                    Eterm::from_raw(0),
                    Eterm::from_raw(0),
                    Eterm::from_raw(bits),
                    base,
                )
            };
            assert_eq!(
                size_object(sub),
                expected,
                "{bits}-bit match context window"
            );
        }
    }

    #[test]
    fn sub_bits_without_a_boxed_origin_reports_its_thing_size() {
        // A non-boxed `orig` is not a term the C accepts: `copy.c:222` dereferences
        // `boxed_val(sb->orig)` unconditionally. Report the thing size instead of
        // following a bogus pointer.
        let mut heap = Heap::new(8);
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let sub = unsafe {
            let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                make_small(3),
            )
        };
        assert_eq!(size_object(sub), ERL_SUB_BITS_SIZE);
    }
}
