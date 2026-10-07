//! `pub(crate)` layout helpers: per-object word counts, per-object child-slot enumeration,
//! off-heap-container identification and the off-heap reference encoding used by the copier.
//!
//! Every boxed shape the copier can meet is sized and walked: transparent tuples/records, funs,
//! flatmaps, **all three HAMT node kinds** (head-array, head-bitmap and inner node-bitmap) and the
//! leaf things (float/bignum/ref/off-heap container/heap-sub bits/external pid/port/ref).
//!
//! Ground truth: `copy.c`'s `move_boxed`/`size_object_x` (the HAMT shape mirrors
//! `size_object_x`, `copy.c:192-214`) and the frozen `BinRef` container thing
//! `[HEADER_BIN_REF, val, next]` (`ERL_BIN_REF_SIZE = 3`, `beam_term::boxed`), where M1 stores
//! the MSO slot in `val` and leaves `next` unused (see the crate `CONTEXT.md`, Design Decisions).
//!
//! The word-level accessors here mirror `beam_gc::copy::move_size`: they are safe `pub(crate)`
//! functions whose raw-pointer preconditions (a live, initialized object of the stated shape) are
//! upheld by the copier, and every dereference carries a `// SAFETY:` rationale.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

/// `FUN_HEADER_ENV_SIZE_OFFS` (`erl_fun.h`): the bit offset of a fun header's free-variable
/// count (`fun_num_free`).
const FUN_HEADER_ENV_SIZE_OFFS: u32 = 16;

/// `fun_num_free(hdr)` — the number of free variables in a fun's environment.
#[inline]
fn fun_num_free(header: Eterm) -> usize {
    ((header.raw() >> FUN_HEADER_ENV_SIZE_OFFS) & 0xFF) as usize
}

/// The number of heap words the object headed by `header` occupies, header word included.
///
/// The *single-object* companion of `beam_term::size_object` (which gives the whole graph's word
/// count): this is what the incremental allocator inside `copy_struct` reserves per object.
/// Ground truth: `copy.c`'s `move_boxed` (the `nelts + 1` sizing).
///
/// All three HAMT node kinds are sized like `beam_gc::copy::move_size`/`beam_term::size_object`
/// (`1 + header_arity + popcount(MAP_HEADER_VAL)`), not collapsed to a leaf.
///
/// `object` must point at the object's header word (i.e. `boxed_val(term)`); it is dereferenced
/// only to read a flatmap's in-payload size word.
pub(crate) fn object_words(header: Eterm, object: *const Eterm) -> usize {
    match header.raw() & TAG_HEADER_MASK {
        ARITYVAL_SUBTAG | RECORD_SUBTAG => header_arity(header) + 1,
        FUN_SUBTAG => ERL_FUN_SIZE + fun_num_free(header),
        MAP_SUBTAG if is_flatmap_header(header) => {
            // `flatmap_t = [thing_word, size, keys, V0..V(size-1)]` => `1 + 1 + 1 + size` words.
            // SAFETY: `object` points at a live flatmap head; word 1 is its untagged size word.
            unsafe { *object.add(1) }.raw() + 3
        }
        MAP_SUBTAG => {
            // Every other map header is a real HAMT node — a head-array, a head-bitmap or an
            // inner node-bitmap — laid out as `[hdr][arity words][n slots]`, one slot per set
            // occupancy bit. `header_arity` is `1` for the two head kinds (their extra `size`
            // word) and `0` for an inner node, so the object is `1 + arity + popcount` words.
            // Mirrors `beam_gc::copy::move_size` and `beam_term::size.rs`'s HAMT arm.
            let arity = header_arity(header);
            let nslots = map_header_val(header).count_ones() as usize;
            1 + arity + nslots
        }
        // Every leaf thing (float, bignum, ref, off-heap container, heap/sub bits, external
        // pid/port/ref): header + `arity` payload words.
        _ => thing_arityval(header) + 1,
    }
}

/// Calls `visit` once per payload word offset of the boxed object headed by `header` at `object`
/// (`boxed_val(term)`) that may hold a term the copier must fix up.
///
/// Offsets are relative to the header word (offset 0). Objects whose payload holds no navigable
/// on-heap child — a float's value, a bignum's limbs, a reference's words, an off-heap container's
/// `val`/`next`, and heap/sub-bit data — yield no offsets. Every **HAMT node kind** (head-array,
/// head-bitmap, inner node-bitmap) instead visits all of its slots: each slot is non-immediate (a
/// leaf cons cell, a boxed sub-node pointer or a collision tuple), so the whole trie is reached.
///
/// `object` must point at the object's header word; it is dereferenced only for a flatmap's size
/// word.
pub(crate) fn for_each_child_slot(
    header: Eterm,
    object: *const Eterm,
    mut visit: impl FnMut(usize),
) {
    match header.raw() & TAG_HEADER_MASK {
        ARITYVAL_SUBTAG | RECORD_SUBTAG => {
            for offset in 1..=header_arity(header) {
                visit(offset);
            }
        }
        FUN_SUBTAG => {
            let num_free = fun_num_free(header);
            for offset in ERL_FUN_SIZE..ERL_FUN_SIZE + num_free {
                visit(offset);
            }
        }
        MAP_SUBTAG if is_flatmap_header(header) => {
            // SAFETY: `object` points at a live flatmap head; word 1 is its untagged size word.
            let size = unsafe { *object.add(1) }.raw();
            visit(2); // the keys tuple
            for offset in 3..3 + size {
                visit(offset);
            }
        }
        MAP_SUBTAG => {
            // A HAMT node — head-array, head-bitmap or inner node-bitmap. Slots start at
            // `1 + header_arity` (after the two head kinds' `size` word) and there is one per set
            // occupancy bit. A collision node is an ordinary ARITYVAL boxed term, walked by the
            // `ARITYVAL_SUBTAG` arm when the copier reaches it, so it needs no special case here.
            let arity = header_arity(header);
            let nslots = map_header_val(header).count_ones() as usize;
            for i in 0..nslots {
                visit(1 + arity + i);
            }
        }
        _ => {}
    }
}

/// Whether `header` is the head word of an off-heap reference container.
pub(crate) fn is_off_heap_container(header: Eterm) -> bool {
    header.raw() == HEADER_BIN_REF.raw()
}

/// Whether the compound term `term` is an off-heap reference container.
///
/// Precondition (as [`is_bin_ref`]): when `term` is boxed, its header word must be a live, aligned
/// term word. The copier only calls this on a word whose primary tag is BOXED.
pub(crate) fn is_off_heap_term(term: Eterm) -> bool {
    if !is_boxed(term) {
        return false;
    }
    // SAFETY: `term` is boxed, so its header word is a live, initialized term word.
    is_off_heap_container(unsafe { *boxed_val(term) })
}

/// Whether the compound term `term` is a full refc binary — the 8-word
/// `[BinRef(3)][ErlSubBits(5)]` pair whose root is the `ErlSubBits` and whose `orig` slot points
/// at the `BinRef`.
///
/// Distinct from the standalone MSO container ([`is_off_heap_term`], a bare `HEADER_BIN_REF`
/// thing whose `val` is an MSO slot): this is the real off-heap binary payload handle.
pub(crate) fn is_refc_binary_term(term: Eterm) -> bool {
    // SAFETY: the copier only calls this on a live boxed word whose header is initialized.
    unsafe { beam_term::refc::is_refc_binary(term) }
}

/// Whether `header` is the head word of an external pid, port or reference (`ExternalThing`).
pub(crate) fn is_external_header(header: Eterm) -> bool {
    let subtag = header.raw() & TAG_HEADER_MASK;
    subtag == EXTERNAL_PID_SUBTAG || subtag == EXTERNAL_PORT_SUBTAG || subtag == EXTERNAL_REF_SUBTAG
}

/// The `(node, creation)` identity of the external pid/port/reference `term`, or `None` when
/// `header` is not an `ExternalThing` head word.
///
/// `term` must be a live external thing. The node is returned as the sysname atom term (the M1
/// substitution `beam_term::thing` documents) and `creation` from the thing's `next` slot.
pub(crate) fn external_node_creation(header: Eterm, term: Eterm) -> Option<(Eterm, u32)> {
    match header.raw() & TAG_HEADER_MASK {
        // SAFETY: `header` proves `term` is a live external thing of that kind, so the
        // `external_*_parts` preconditions hold.
        EXTERNAL_PID_SUBTAG => {
            let (node, _number, _serial, creation) = unsafe { external_pid_parts(term) };
            Some((node, creation))
        }
        EXTERNAL_PORT_SUBTAG => {
            let (node, _number, creation) = unsafe { external_port_parts(term) };
            Some((node, creation))
        }
        EXTERNAL_REF_SUBTAG => {
            let (node, _ids, creation) = unsafe { external_ref_parts(term) };
            Some((node, creation))
        }
        _ => None,
    }
}

/// The off-heap slot stored in the container `container` (its `val` word).
///
/// Precondition: `container` is a live off-heap reference container (`is_off_heap_term`).
pub(crate) fn off_heap_slot(container: Eterm) -> usize {
    // SAFETY: `container` is a live `BinRef` thing; word 1 is its `val`.
    unsafe { *boxed_val(container).add(1) }.raw()
}

/// Builds an off-heap reference container for `slot` on the destination heap `to`.
///
/// The M1 encoding is the frozen `BinRef` thing `[HEADER_BIN_REF, val, next]` where `val` is the
/// MSO slot index in the owning process's off-heap table and `next` is unused (`0`).
pub(crate) fn make_off_heap_container(to: &mut Heap, slot: usize) -> Result<Eterm> {
    // SAFETY: `alloc_words` returns `ERL_BIN_REF_SIZE` exclusively-owned, writable words.
    let p = unsafe { to.alloc_words(ERL_BIN_REF_SIZE)? };
    // SAFETY: `p` is the base of those three words.
    unsafe {
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
    }
    Ok(make_boxed(p as *const Eterm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_words_counts_header_plus_payload() {
        // A stack word the pointer arguments may point at; the non-flatmap arms never read it.
        let scratch = [Eterm::from_raw(0)];
        let ptr = scratch.as_ptr();

        // FLONUM: header + 1 => 2 words.
        assert_eq!(object_words(HEADER_FLONUM, ptr), 2);
        // BIN_REF: header + 2 => 3 words.
        assert_eq!(object_words(HEADER_BIN_REF, ptr), 3);
        // Sub-bits: header arity 4 => 5 words.
        assert_eq!(object_words(HEADER_SUB_BITS, ptr), 5);
        // A tuple header of arity 3 => 4 words.
        assert_eq!(object_words(make_arityval(3), ptr), 4);
        // A fun header: ERL_FUN_SIZE (2) + `num_free` (env-size bits at offset 16).
        let fun_hdr = Eterm::from_raw(FUN_SUBTAG | (2usize << 8) | (3usize << 16));
        assert_eq!(object_words(fun_hdr, ptr), ERL_FUN_SIZE + 3);
    }

    #[test]
    fn object_words_of_a_flatmap_reads_the_size_word() {
        // `flatmap_t` of size 2: [thing_word(type=0, arity=1), size=2, keys, V0, V1].
        let words = [
            make_header(1usize << 2, MAP_SUBTAG),
            Eterm::from_raw(2),
            make_small(0),
            make_small(1),
            make_small(2),
        ];
        assert!(is_flatmap_header(words[0]));
        assert_eq!(object_words(words[0], words.as_ptr()), 5);
    }

    #[test]
    fn child_slots_follow_the_thing_layout() {
        let scratch = [Eterm::from_raw(0)];
        let ptr = scratch.as_ptr();

        let mut tuple = Vec::new();
        for_each_child_slot(make_arityval(3), ptr, |offset| tuple.push(offset));
        assert_eq!(tuple, vec![1, 2, 3]);

        let fun_hdr = Eterm::from_raw(FUN_SUBTAG | (2usize << 8) | (1usize << 16));
        let mut fun = Vec::new();
        for_each_child_slot(fun_hdr, ptr, |offset| fun.push(offset));
        assert_eq!(fun, vec![ERL_FUN_SIZE]);

        let mut none = Vec::new();
        for_each_child_slot(HEADER_FLONUM, ptr, |offset| none.push(offset));
        assert!(none.is_empty(), "a leaf thing has no navigable child");
    }

    #[test]
    fn child_slots_of_a_flatmap_are_keys_and_values() {
        let words = [
            make_header(1usize << 2, MAP_SUBTAG),
            Eterm::from_raw(2),
            make_small(0),
            make_small(1),
            make_small(2),
        ];
        let mut slots = Vec::new();
        for_each_child_slot(words[0], words.as_ptr(), |offset| slots.push(offset));
        assert_eq!(slots, vec![2, 3, 4]);
    }

    #[test]
    fn object_words_of_hamt_nodes_is_header_arity_plus_slots() {
        // Head-array: `[hdr][size][16 slots]` — 18 words (`map_header_val = 0xffff`).
        let head_array = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff); 18];
        assert!(is_map_header(head_array[0]) && !is_flatmap_header(head_array[0]));
        assert_eq!(object_words(head_array[0], head_array.as_ptr()), 18);

        // Head-bitmap with two set bits: `[hdr][size][slot][slot]` — `2 + nslots` words.
        let head_bitmap = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0011); 4];
        assert_eq!(object_words(head_bitmap[0], head_bitmap.as_ptr()), 2 + 2);

        // Inner node-bitmap with three set bits: `[hdr][slot][slot][slot]` — `1 + nslots` words.
        let inner = [make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b1011); 4];
        assert_eq!(object_words(inner[0], inner.as_ptr()), 1 + 3);
    }

    #[test]
    fn child_slots_of_hamt_nodes_are_the_occupied_slots() {
        // Head-array: one slot per word 2..18.
        let head_array = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff); 18];
        let mut slots = Vec::new();
        for_each_child_slot(head_array[0], head_array.as_ptr(), |o| slots.push(o));
        assert_eq!(slots, (2..18).collect::<Vec<_>>());

        // Head-bitmap (two set bits): slots start after the head's `size` word.
        let head_bitmap = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0101); 4];
        let mut slots = Vec::new();
        for_each_child_slot(head_bitmap[0], head_bitmap.as_ptr(), |o| slots.push(o));
        assert_eq!(slots, vec![2, 3]);

        // Inner node-bitmap (three set bits): no `size` word, slots start at word 1.
        let inner = [make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b1011); 4];
        let mut slots = Vec::new();
        for_each_child_slot(inner[0], inner.as_ptr(), |o| slots.push(o));
        assert_eq!(slots, vec![1, 2, 3]);
    }

    #[test]
    fn off_heap_container_round_trip() {
        let mut heap = Heap::new(8).expect("test heap");
        let term = make_off_heap_container(&mut heap, 5).expect("the bin-ref fits");
        assert!(is_off_heap_term(term));
        // SAFETY: `term` is a live off-heap container whose header word is readable.
        assert!(is_off_heap_container(unsafe { *boxed_val(term) }));
        assert_eq!(off_heap_slot(term), 5);
        assert_eq!(heap.used(), ERL_BIN_REF_SIZE);
    }

    #[test]
    fn non_container_terms_are_not_off_heap() {
        let mut heap = Heap::new(8).expect("test heap");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        assert!(!is_off_heap_term(tuple));
        assert!(!is_off_heap_term(make_small(1)));
        assert!(!is_off_heap_term(NIL));
    }
}
