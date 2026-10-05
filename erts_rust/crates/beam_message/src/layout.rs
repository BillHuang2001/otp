//! `pub(crate)` layout helpers: per-object word counts, off-heap-container identification and
//! the off-heap reference encoding used by the copier.
//!
//! Ground truth: `copy.c`'s `move_boxed`/`size_object_x` and the frozen `BinRef` container thing
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
        // Non-flatmap map heads (hashmaps) and every leaf thing (float, bignum, ref, off-heap
        // container, heap/sub bits, external pid/port/ref): header + `arity` payload words.
        _ => thing_arityval(header) + 1,
    }
}

/// Calls `visit` once per payload word offset of the boxed object headed by `header` at `object`
/// (`boxed_val(term)`) that may hold a term the copier must fix up.
///
/// Offsets are relative to the header word (offset 0). Objects whose payload holds no navigable
/// on-heap child — a float's value, a bignum's limbs, a reference's words, an off-heap container's
/// `val`/`next`, heap/sub-bit data and a (non-flatmap) hashmap head — yield no offsets.
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
