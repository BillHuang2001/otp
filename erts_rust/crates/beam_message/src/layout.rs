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

//! `pub(crate)` layout helpers: per-object word counts, per-object child-slot enumeration,
//! off-heap-container identification and the off-heap reference encoding used by the copier.
//!
//! Every boxed shape the copier can meet is sized and walked: transparent tuples/records, funs,
//! flatmaps, **all three HAMT node kinds** (head-array, head-bitmap and inner node-bitmap), an
//! **`ErlSubBits`** (sized as the whole 8-word refc-binary pair, or as the flattened match-context
//! window the copier writes — so it is not a plain leaf) and the leaf things
//! (float/bignum/ref/off-heap container/heap bits/external pid/port/ref).
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

/// The word offset of `env[0]` in the fun block headed by `header`: `1 + desc_words(kind)`.
///
/// Derived from the **frozen** fun layout owned by `beam_term::fun` (the M1 inlined-descriptor
/// substitution replaces the C `entry` pointer with `desc_words(kind)` descriptor words), and
/// **not** from the legacy C `ERL_FUN_SIZE = 2`, which would land on the second descriptor word
/// (a raw `index`/`uniq`/module word) instead of the environment.
///
/// Ground truth: `beam_term::fun::fun_env_offset_header` / `fun_desc_words_header`, mirroring
/// `beam_gc::copy::scan_step`.
#[inline]
fn fun_env_offset(header: Eterm) -> usize {
    // `fun_kind_of_header` is the infallible bit extraction and is valid because every caller
    // tests `FUN_SUBTAG` first; no `Result` on a path that cannot fail.
    1 + fun_desc_words(fun_kind_of_header(header))
}

/// The words the fun block headed by `header` occupies, header word included:
/// `1 + desc_words(kind) + num_free`.
///
/// Ground truth: `beam_term::fun::fun_words` / `fun_block_words_header` (and
/// `beam_gc::copy::move_size`); deliberately not `thing_arityval`-based, because a fun header
/// carries the C `ERL_FUN_SIZE - 1` value in its arity field.
#[inline]
fn fun_block_words(header: Eterm) -> usize {
    fun_words(fun_kind_of_header(header), fun_num_free(header))
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
        FUN_SUBTAG => fun_block_words(header),
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
        SUB_BITS_SUBTAG => {
            // `[HEADER_SUB_BITS, base_flags, start, end, orig]` (`copy.c:219-242`) has two shapes,
            // and this mirrors what `copy_struct` actually writes for each:
            //
            // * a **refc binary** (`orig` is a live `BinRef`) is the whole 8-word
            //   `[BinRef(3)][ErlSubBits(5)]` pair — `copy_refc_binary` materialises all 8 words
            //   (`copy.c:224-231`);
            // * anything else is a **match context** over an on-heap bitstring, which `shallow`
            //   *flattens* into a fresh `heap_bits_size(size)` block (`copy.c:232-239`).
            //
            // SAFETY: `object` points at a live, initialized `ErlSubBits` head (the copier's
            // invariant for every arm here), so words 1..4 of the block are live.
            let (_base_flags, start, end, orig) = unsafe { sub_bits_parts(make_boxed(object)) };
            // SAFETY: `orig` is guarded by `is_boxed_term`, so `boxed_val` dereferences only a
            // live boxed term word (`orig` is an on-heap bitstring for a match context).
            let is_refc = is_boxed_term(orig) && unsafe { *boxed_val(orig) } == HEADER_BIN_REF;
            if is_refc {
                ERL_REFC_BITS_SIZE
            } else {
                // A match context over an on-heap bitstring: the copy materialises a fresh
                // heap-bits block for the window (`copy.c:232-239`), sized exactly as
                // `beam_term::size::describe_object` counts it.
                heap_bits_size_words(end.raw().saturating_sub(start.raw()))
            }
        }
        // Every leaf thing (float, bignum, ref, off-heap container, heap bits, external
        // pid/port/ref): header + `arity` payload words.
        _ => thing_arityval(header) + 1,
    }
}

/// Calls `visit` once per payload word offset of the boxed object headed by `header` at `object`
/// (`boxed_val(term)`) that may hold a term the copier must fix up.
///
/// Offsets are relative to the header word (offset 0). Objects whose payload holds no navigable
/// on-heap child — a float's value, a bignum's limbs, a reference's words, an off-heap container's
/// `val`/`next`, heap bits, and an `ErlSubBits` (the `SUB_BITS` arm yields no offsets: the copier
/// flattens a match context and copies a refc binary's `BinRef` itself) — yield no offsets. Every
/// **HAMT node kind** (head-array, head-bitmap, inner node-bitmap) instead visits all of its slots:
/// each slot is non-immediate (a leaf cons cell, a boxed sub-node pointer or a collision tuple), so
/// the whole trie is reached.
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
            // `env[i]` lives at `1 + desc_words(kind) + i` (the frozen `beam_term::fun` layout),
            // never at the legacy C `ERL_FUN_SIZE`.
            let env = fun_env_offset(header);
            for offset in env..env + fun_num_free(header) {
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
        SUB_BITS_SUBTAG => {
            // An `ErlSubBits` exposes **no** navigable child. A **match context** is *flattened*
            // by `copy_struct` into an opaque on-heap-bitstring leaf (the window's bits are copied
            // out; its `orig` is never followed), and a **refc binary**'s `BinRef` is copied
            // together with the `ErlSubBits` by `copy_refc_binary`, which also writes the copied
            // `orig` — so neither is a fix-up slot the caller's worklist must resolve.
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

/// Whether `term` is a `SUB_BITS` **match context** — an `ErlSubBits` that is *not* a refc binary
/// (its `orig` is an on-heap bitstring rather than a `BinRef`).
///
/// `copy_struct` **flattens** such a value into a fresh on-heap bitstring (`copy.c:849-863`)
/// rather than copying the window's `base_flags`/`orig` pointers verbatim, which would leave the
/// destination term pointing into the source process's heap (a dangling reference once the source
/// is collected or read). A refc binary is handled separately by `copy_refc_binary` (it shares its
/// off-heap payload and fixes `orig` up itself), so it is excluded here.
pub(crate) fn is_match_context_term(term: Eterm) -> bool {
    if !is_boxed_term(term) {
        return false;
    }
    // SAFETY: `term` is a boxed term, so its header word is a live term word.
    if unsafe { *boxed_val(term) } != HEADER_SUB_BITS {
        return false;
    }
    // SAFETY: the header fixed `term` at the 5-word `ErlSubBits` layout, so its `orig` slot is
    // live; `is_refc_binary` reads it and dereferences only a boxed `BinRef`.
    !unsafe { beam_term::refc::is_refc_binary(term) }
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
        // A tuple header of arity 3 => 4 words.
        assert_eq!(object_words(make_arityval(3), ptr), 4);
        // A fun header: `1 + desc_words(kind) + num_free` from the frozen `beam_term::fun`
        // layout — 6 words for a `Local` fun (2 descriptor words: uniq, index) with 3 free vars.
        let fun_hdr = make_fun_header(2, 3, FunKind::Local).expect("a valid fun header");
        assert_eq!(object_words(fun_hdr, ptr), 1 + 2 + 3);
        assert_eq!(object_words(fun_hdr, ptr), fun_words(FunKind::Local, 3));
        // An `EXPORT_EXT` fun carries 2 descriptor words and no environment.
        let ext_hdr = make_fun_header(1, 0, FunKind::External).expect("a valid fun header");
        assert_eq!(object_words(ext_hdr, ptr), 1 + 2);
        // A `NEW_FUN_EXT` fun carries 10 descriptor words.
        let imp_hdr = make_fun_header(1, 2, FunKind::Imported).expect("a valid fun header");
        assert_eq!(object_words(imp_hdr, ptr), 1 + 10 + 2);
        assert_eq!(object_words(imp_hdr, ptr), fun_words(FunKind::Imported, 2));
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
    fn object_words_of_a_sub_bits_matches_the_copy_shape() {
        // A **match context** over an on-heap bitstring: `orig` is a boxed `ErlHeapBits`, so the
        // copy flattens the window into a `heap_bits_size(end - start)` block.
        let heap_bits = [
            make_header(2, HEAP_BITS_SUBTAG),
            Eterm::from_raw(16),
            Eterm::from_raw(0),
        ];
        let match_context = [
            HEADER_SUB_BITS,
            Eterm::from_raw(0),             // base_flags (unused for sizing)
            Eterm::from_raw(3),             // start
            Eterm::from_raw(3 + 21),        // end
            make_boxed(heap_bits.as_ptr()), // orig = an on-heap bitstring
        ];
        assert_eq!(
            object_words(match_context[0], match_context.as_ptr()),
            heap_bits_size_words(21)
        );
        assert!(
            is_match_context_term(make_boxed(match_context.as_ptr())),
            "a `SUB_BITS` whose `orig` is an on-heap bitstring is a match context"
        );

        // A **refc binary**: `orig` is a boxed `BinRef`, so the whole 8-word pair is copied.
        let bin_ref = [HEADER_BIN_REF, Eterm::from_raw(0), Eterm::from_raw(0)];
        let refc = [
            HEADER_SUB_BITS,
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(32),
            make_boxed(bin_ref.as_ptr()),
        ];
        assert_eq!(object_words(refc[0], refc.as_ptr()), ERL_REFC_BITS_SIZE);
        assert!(!is_match_context_term(make_boxed(refc.as_ptr())));
        assert!(is_refc_binary_term(make_boxed(refc.as_ptr())));

        // Neither `ErlSubBits` shape exposes a navigable child offset.
        for block in [&match_context[..], &refc[..]] {
            let mut slots = Vec::new();
            for_each_child_slot(block[0], block.as_ptr(), |offset| slots.push(offset));
            assert!(slots.is_empty(), "an `ErlSubBits` exposes no child slot");
        }
    }

    #[test]
    fn child_slots_follow_the_thing_layout() {
        let scratch = [Eterm::from_raw(0)];
        let ptr = scratch.as_ptr();

        let mut tuple = Vec::new();
        for_each_child_slot(make_arityval(3), ptr, |offset| tuple.push(offset));
        assert_eq!(tuple, vec![1, 2, 3]);

        let fun_hdr = make_fun_header(1, 1, FunKind::Local).expect("a valid fun header");
        let mut fun = Vec::new();
        for_each_child_slot(fun_hdr, ptr, |offset| fun.push(offset));
        assert_eq!(
            fun,
            vec![1 + 2],
            "`env[0]` is at `1 + desc_words(Local)`, not `ERL_FUN_SIZE`"
        );

        let imp_hdr = make_fun_header(1, 2, FunKind::Imported).expect("a valid fun header");
        let mut imp_env = Vec::new();
        for_each_child_slot(imp_hdr, ptr, |offset| imp_env.push(offset));
        assert_eq!(
            imp_env,
            vec![1 + 10, 1 + 10 + 1],
            "an imported fun's env follows 10 words"
        );

        let ext_hdr = make_fun_header(1, 0, FunKind::External).expect("a valid fun header");
        let mut no_env = Vec::new();
        for_each_child_slot(ext_hdr, ptr, |offset| no_env.push(offset));
        assert!(
            no_env.is_empty(),
            "an `EXPORT_EXT` fun has no environment and no navigable child"
        );

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
