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
//! # Known Issues
//!
//! The C walks every internal layout. This port faithfully covers the layouts
//! the rewrite needs today and *documents* (rather than silently guessing) the
//! remaining approximations:
//!
//! * **Hashmaps** (`MAP_HEADER_TAG_HAMT_*`) are approximated by the head node's
//!   `arity + 1` words; the deep HAMT nodes are not counted.
//! * **Sub-bitstrings** (`SUB_BITS_SUBTAG`) report the on-heap `ErlSubBits`
//!   thing size (`arity + 1`); the C reports the size of the *copied*
//!   representation (`ERL_REFC_BITS_SIZE` / the underlying heap bits).
//! * **Native records** (`RECORD_SUBTAG`) are treated like tuples (their
//!   field-table layout is not yet modelled).

use crate::boxed::{
    boxed_val, header_arity, is_flatmap_header, is_hashmap_header, map_header_val, thing_arityval,
    ERL_FUN_SIZE,
};
use crate::eterm::Eterm;
use crate::immediate::is_immediate;
use crate::list::{car, cdr, is_list};
use crate::tag::{ARITYVAL_SUBTAG, FUN_SUBTAG, MAP_SUBTAG, RECORD_SUBTAG, TAG_HEADER_MASK};

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
                    } else if is_hashmap_header(header) {
                        // Hash-map node: `[thing_word, size?, slots...]`.
                        // `header_arity(header)` is `1` for the head nodes (their
                        // extra `size` word) and `0` for inner node-bitmap nodes,
                        // so the slots start at `1 + header_arity`. Mirrors
                        // `size_object_x` (`copy.c:192-214`):
                        // `sum += 1 + popcount(MAP_HEADER_VAL) + header_arity`.
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
                    } else {
                        // Not a map at all (forged term): header + payload.
                        total += thing_arityval(header) + 1;
                    }
                }
                _ => {
                    // Float, bignum, ref, bin ref, heap/sub bits, external
                    // pid/port/ref: header + payload. None of these (except the
                    // documented sub-bits approximation) has navigable subterms.
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
    use crate::boxed::{make_boxed, make_header, HEADER_FLONUM};
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small, NIL};
    use crate::tag::HEAP_BITS_SUBTAG;
    use crate::tuple::tuple0;

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
}
