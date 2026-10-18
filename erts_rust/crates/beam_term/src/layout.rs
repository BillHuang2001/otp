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

//! Canonical **per-object layout** — the single source of truth for how many heap
//! words one object occupies and which of its payload words are term references a
//! copier must forward.
//!
//! This module is the lowest-common-ancestor owner of the per-object walk. Every
//! copier/sizer in the workspace (`beam_term::size`, `beam_message::layout`,
//! `beam_gc::copy`) reproduces the same two questions for each boxed shape:
//!
//! 1. **how many words does the object occupy?** — [`object_words`];
//! 2. **which of its payload words are references a copier must re-point?** — the
//!    child-slot enumerators.
//!
//! Keeping both answers here (rather than transcribed per crate) is what stops the
//! three copies drifting apart; this crate already owns the frozen term layout
//! (workspace `CONTEXT.md` §2), so it owns the derived per-object walk too.
//!
//! # The two halves of the walk, and where they diverge
//!
//! The *sizing* half ([`crate::size::size_object`]) asks how many words the whole
//! graph occupies; the *copy* half asks a copier which words to copy and which to
//! forward. For every shape but [`SUB_BITS_SUBTAG`] the two agree exactly. For an
//! `ErlSubBits` they deliberately **diverge**, and the divergence is the reason the
//! child enumeration is split into two functions:
//!
//! * [`for_each_sizing_child_slot`] — the **sizing** children. An `ErlSubBits` exposes
//!   *no* child: `size_object_x` counts its contribution opaquely (`copy.c:219-242`)
//!   and never descends into `orig` — descending would double-count (for a refc binary
//!   the `BinRef` is already inside the `ERL_REFC_BITS_SIZE` total).
//! * [`for_each_child_slot`] — the **copy** children. An `ErlSubBits` exposes its
//!   `orig` word (offset [`SUB_BITS_ORIG_WORD`]): it is a *boxed reference* the copier
//!   re-points when it copies the `ErlSubBits` block verbatim. For a refc binary that
//!   child is the `BinRef` the pair's `orig` slot names.
//!
//! # `object_words`: the word count
//!
//! [`object_words`] returns the words the object *contributes to the flat graph size*,
//! the **header word included**, for an object whose header word is `header` and whose
//! header address is `ptr`:
//!
//! | shape | words |
//! |---|---|
//! | tuple / native record (`ARITYVAL_SUBTAG` / `RECORD_SUBTAG`) | `arity + 1` |
//! | fun (`FUN_SUBTAG`), all three kinds | `1 + desc_words(kind) + num_free` |
//! | flatmap (map with `MAP_HEADER_TAG_FLATMAP_HEAD`) | `size + 3` |
//! | HAMT node (head-array / head-bitmap / inner node-bitmap) | `1 + header_arity + popcount(MAP_HEADER_VAL)` |
//! | any other thing (float, bignum, ref, `BinRef`, heap-bits, external pid/port/ref) | `thing_arityval + 1` |
//! | `ErlSubBits`, `orig` is a `BinRef` (a real refc binary) | `ERL_REFC_BITS_SIZE` (`8`) |
//! | `ErlSubBits`, `orig` is an on-heap bitstring (a match context) | `heap_bits_size_words(end - start)` |
//! | `ErlSubBits`, `orig` is not a boxed term (standalone/malformed) | `ERL_SUB_BITS_SIZE` (`5`) |
//!
//! Two `ErlSubBits` subtleties are worth stating explicitly:
//!
//! * **The refc pair is not contiguous with its root.** A refc binary is the 8-word
//!   block `[BinRef(3)][ErlSubBits(5)]` whose *tagged root is the `ErlSubBits`* at
//!   `ptr`; its `BinRef` sits at offset `-3`. `object_words` therefore returns the
//!   whole pair's words (`8`) — the amount a destination must reserve — but a copier
//!   must **not** `copy` 8 words starting at `ptr`: it copies the `ErlSubBits`'s own
//!   [`ERL_SUB_BITS_SIZE`] words and forwards the `orig` word, which copies the 3-word
//!   `BinRef` as its own child (the C `copy_struct_x` protocol, `copy.c:1677-1700`).
//! * **The empty-tuple literal is the caller's special case.** `object_words` returns
//!   `arity + 1` uniformly for `ARITYVAL_SUBTAG`/`RECORD_SUBTAG`, so the shared
//!   zero-arity tuple (`tuple0()`) reports `1` word **and so does a zero-arity
//!   `RECORD` header**. The `size_object_x` walk prices `0` for exactly one object: a
//!   **literal-tagged** arity-0 `ARITYVAL` — the global empty tuple
//!   (`ERTS_GLOBAL_LIT_EMPTY_TUPLE`, `copy.c:125-140`), whose `ASSERT` there explicitly
//!   excludes a record header (`copy.c:128`). Callers that must reproduce
//!   `size_object` therefore skip that one literal before calling `object_words` (as
//!   [`crate::size`] and the `beam_message` copier do).
//!
//! # The child-slot enumerators
//!
//! Both enumerators call `visit` with **word offsets relative to the object's header
//! word** (offset `0` = the header). A cons cell has no header word and is therefore
//! not part of this header-based API: a `TAG_PRIMARY_LIST` term is `2` words whose two
//! payload words are both references, and every caller handles it directly.
//!
//! The offsets of one object form a **single ascending contiguous run** (`first ..
//! first + count`), which is what lets a caller reconstruct the run as a `(base,
//! count)` slice. Shapes:
//!
//! | shape | sizing children | copy children |
//! |---|---|---|
//! | tuple / native record | `1 ..= arity` | same |
//! | fun | `env .. env + num_free` (`env = 1 + desc_words(kind)`) | same |
//! | flatmap | `2` (the keys tuple) then `3 .. 3 + size` (the values) | same |
//! | HAMT node | `1 + header_arity .. + popcount(MAP_HEADER_VAL)` | same |
//! | `ErlSubBits` | none | `SUB_BITS_ORIG_WORD` (`4`) |
//! | float / bignum / ref / `BinRef` / heap-bits / external pid-port-ref | none | none |
//!
//! Ground truth: `size_object_x` (`copy.c:88-263`, the sizing half) and
//! `move_boxed`/`copy_struct_x` (`copy.c`, the copying half); the frozen fun layout is
//! [`crate::fun`] (the M1 inlined-descriptor substitution for the C `entry` pointer)
//! and the map node layouts are [`crate::map`].
//!
//! # `unsafe` contract
//!
//! Every entry point here is `unsafe`: it dereferences raw header/payload words. The
//! invariant is the one [`crate::compare::cmp`] and every other materialization entry
//! point in this crate document — the caller passes a live header word `header` at
//! `ptr`, and every payload word the shape implies is live and readable. A word that is
//! **not a term** (a bare header word such as the ABI's `THE_NON_VALUE`) must never be
//! dereferenced this way — these functions are the header-based half of a walk whose
//! term-level entry point ([`crate::size::size_object`]) already rejects non-terms.

use crate::bitstring::{
    heap_bits_size_words, SUB_BITS_END_WORD, SUB_BITS_ORIG_WORD, SUB_BITS_START_WORD,
};
use crate::boxed::{
    boxed_val, header_arity, is_boxed_term, is_flatmap_header, map_header_val, thing_arityval,
    ERL_SUB_BITS_SIZE, HEADER_BIN_REF,
};
use crate::eterm::Eterm;
use crate::fun::{fun_desc_words, fun_kind_of_header, fun_num_free, fun_words};
use crate::refc::ERL_REFC_BITS_SIZE;
use crate::tag::{
    ARITYVAL_SUBTAG, FUN_SUBTAG, MAP_SUBTAG, RECORD_SUBTAG, SUB_BITS_SUBTAG, TAG_HEADER_MASK,
};

/// The number of heap words the boxed object headed by `header` occupies, the header
/// word included — the object's contribution to the flat graph size.
///
/// See the [module docs](self) for the per-shape table and, in particular, the
/// `ErlSubBits` cases (a refc binary reports the whole `[BinRef][ErlSubBits]` pair, not
/// the `ErlSubBits` block alone).
///
/// # Safety
///
/// `ptr` must address the live, aligned header word `header` of a live, initialized
/// object of that shape. `ptr` is read only for a flatmap's untagged `size` word and
/// for an `ErlSubBits`'s `start`/`end`/`orig` words (`header` proves those words are
/// present). A non-term word must never be passed.
pub unsafe fn object_words(header: Eterm, ptr: *const Eterm) -> usize {
    match header.raw() & TAG_HEADER_MASK {
        ARITYVAL_SUBTAG | RECORD_SUBTAG => header_arity(header) + 1,
        // A fun is `1 + desc_words(kind) + num_free`, **not** `thing_arityval + 1`: its
        // header arity field carries the C `ERL_FUN_SIZE - 1` (see `crate::fun`).
        FUN_SUBTAG => fun_words(fun_kind_of_header(header), fun_num_free(header)),
        MAP_SUBTAG if is_flatmap_header(header) => {
            // `flatmap_t = [thing_word, size, keys, V0..V(size-1)]` => `size + 3` words.
            // SAFETY: `ptr` addresses a live flatmap head; word 1 is its untagged size word.
            unsafe { *ptr.add(1) }.raw() + 3
        }
        MAP_SUBTAG => {
            // A HAMT node — head-array, head-bitmap or inner node-bitmap — is
            // `[hdr][arity words][n slots]`, one slot per set occupancy bit. `header_arity`
            // is `1` for the two head kinds (their extra `size` word) and `0` for an inner
            // node. Mirrors `size_object_x` (`copy.c:192-214`).
            let arity = header_arity(header);
            let nslots = map_header_val(header).count_ones() as usize;
            1 + arity + nslots
        }
        // SAFETY: `header` proves `ptr` heads a live `ErlSubBits`.
        SUB_BITS_SUBTAG => unsafe { sub_bits_words(ptr) },
        // Every other thing (float, bignum, ref, `BinRef`, heap-bits, external
        // pid/port/ref): header + `arity` payload words.
        _ => thing_arityval(header) + 1,
    }
}

/// The words an `ErlSubBits` (`ptr` = its header address) contributes, per the three
/// `size_object_x` cases (`copy.c:219-242`).
///
/// # Safety
///
/// `ptr` must address a live `ErlSubBits` header word, so words 1..4 (`base_flags`,
/// `start`, `end`, `orig`) are initialized; if `orig` is boxed, its header word must be
/// live (`copy.c:222` reads `*boxed_val(sb->orig)` unconditionally).
unsafe fn sub_bits_words(ptr: *const Eterm) -> usize {
    // SAFETY: `ptr` heads a live `ErlSubBits`, so its `start`/`end`/`orig` words are
    // initialized.
    let (start, end, orig) = unsafe {
        (
            *ptr.add(SUB_BITS_START_WORD),
            *ptr.add(SUB_BITS_END_WORD),
            *ptr.add(SUB_BITS_ORIG_WORD),
        )
    };
    if is_boxed_term(orig) {
        // SAFETY: `orig` is a boxed term, so its header word is a live term word.
        let underlying = unsafe { *boxed_val(orig) };
        if underlying == HEADER_BIN_REF {
            // A full refc binary: the whole on-heap `[BinRef(3)][ErlSubBits(5)]` pair is
            // counted in one go (`copy.c:224-231`); the pair's `BinRef` is not descended
            // into separately.
            ERL_REFC_BITS_SIZE
        } else {
            // A match context over an on-heap bitstring: the copy materialises a fresh
            // heap-bits block for the window (`copy.c:232-239`).
            heap_bits_size_words(end.raw().saturating_sub(start.raw()))
        }
    } else {
        // Not a term the C accepts (`boxed_val(sb->orig)` would dereference a bogus
        // pointer there). Report the thing size instead of following a bogus pointer.
        ERL_SUB_BITS_SIZE
    }
}

/// The single contiguous run `(first_offset, count)` of an object's **sizing** child
/// slots, or `None` when it exposes none.
///
/// Shared by [`for_each_sizing_child_slot`] and the copy enumeration (which adds the
/// one `ErlSubBits` override).
///
/// # Safety
///
/// As [`object_words`]: `ptr` must address the live header word `header`.
unsafe fn sizing_child_slots(header: Eterm, ptr: *const Eterm) -> Option<(usize, usize)> {
    match header.raw() & TAG_HEADER_MASK {
        ARITYVAL_SUBTAG | RECORD_SUBTAG => {
            // The elements follow the header; an arity of 0 (the shared empty-tuple
            // literal) exposes none.
            let arity = header_arity(header);
            (arity > 0).then_some((1, arity))
        }
        FUN_SUBTAG => {
            // `env[i]` lives at `1 + desc_words(kind) + i` (the frozen `crate::fun`
            // layout), never at the legacy C `ERL_FUN_SIZE`.
            let count = fun_num_free(header);
            let env = 1 + fun_desc_words(fun_kind_of_header(header));
            (count > 0).then_some((env, count))
        }
        MAP_SUBTAG if is_flatmap_header(header) => {
            // Word 2 is the keys tuple, followed by the `size` values — contiguous from
            // word 2 through word `2 + size`.
            // SAFETY: `ptr` addresses a live flatmap head; word 1 is its untagged size word.
            let size = unsafe { *ptr.add(1) }.raw();
            Some((2, size + 1))
        }
        MAP_SUBTAG => {
            // A HAMT node: slots start at `1 + header_arity` (after the two head kinds'
            // `size` word), one per set occupancy bit. A collision node is an ordinary
            // ARITYVAL boxed term, walked by the `ARITYVAL_SUBTAG` arm when reached.
            let nslots = map_header_val(header).count_ones() as usize;
            let first = 1 + header_arity(header);
            (nslots > 0).then_some((first, nslots))
        }
        // An `ErlSubBits` and every leaf thing expose no sizing child.
        _ => None,
    }
}

/// Calls `visit` once per **sizing** child-slot offset of the object headed by `header`
/// at `ptr`.
///
/// Offsets are word offsets relative to the header word (offset `0`), yielded in
/// ascending order as one contiguous run — see the [module docs](self) for the
/// per-shape table. An `ErlSubBits` exposes **no** child here (its contribution is
/// counted opaquely; see [`for_each_child_slot`] for the copy-walk divergence).
///
/// # Safety
///
/// `ptr` must address the live header word `header` of a live object whose payload words
/// the shape implies are readable, and `visit` must not invalidate them.
pub unsafe fn for_each_sizing_child_slot(
    header: Eterm,
    ptr: *const Eterm,
    mut visit: impl FnMut(usize),
) {
    // SAFETY: the caller guarantees `ptr` heads a live object of `header`'s shape.
    if let Some((first, count)) = unsafe { sizing_child_slots(header, ptr) } {
        for offset in first..first + count {
            visit(offset);
        }
    }
}

/// Calls `visit` once per **copy** child-slot offset of the object headed by `header`
/// at `ptr` — the slots a copier must forward.
///
/// Identical to [`for_each_sizing_child_slot`] except for an `ErlSubBits`, whose `orig`
/// word (offset [`SUB_BITS_ORIG_WORD`] = `4`) *is* reported: it is a boxed reference the
/// copier re-points when it copies the `ErlSubBits` block verbatim (the `BinRef` of a
/// refc binary, or the underlying on-heap bitstring of a match context). This is the
/// documented sizing/copying divergence — see the [module docs](self).
///
/// Offsets are word offsets relative to the header word (offset `0`).
///
/// # Safety
///
/// As [`for_each_sizing_child_slot`]: `ptr` must address the live header word `header` of
/// a live object, and `visit` must not invalidate its words. For an `ErlSubBits`, word
/// `4` is `orig`, which may be a boxed reference whose target is live.
pub unsafe fn for_each_child_slot(
    header: Eterm,
    ptr: *const Eterm,
    mut visit: impl FnMut(usize),
) {
    if header.raw() & TAG_HEADER_MASK == SUB_BITS_SUBTAG {
        // SAFETY: `header` proves the block is a live `ErlSubBits`, so word 4 is `orig`.
        let _orig = unsafe { *ptr.add(SUB_BITS_ORIG_WORD) };
        visit(SUB_BITS_ORIG_WORD);
        return;
    }
    // SAFETY: the caller guarantees `ptr` heads a live object of `header`'s shape.
    if let Some((first, count)) = unsafe { sizing_child_slots(header, ptr) } {
        for offset in first..first + count {
            visit(offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    use crate::bitstring::{write_heap_bits, write_sub_bits};
    use crate::boxed::{
        is_map_header, make_arityval, make_arityval_zero, make_header, make_record_header,
        HEADER_FLONUM,
    };
    use crate::fun::{make_fun_header, write_fun_imported, write_fun_local, FunKind, FunPid};
    use crate::heap::Heap;
    use crate::immediate::{is_immediate, make_atom, make_small, NIL};
    use crate::list::{is_list, list_val};
    use crate::map::{make_map_header, map_words_capacity, write_map};
    use crate::record::{record_block_words, record_inst_size_words, write_record};
    use crate::refc::write_refc_binary;
    use crate::size::size_object;
    use crate::tag::{
        is_term_word, MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
        MAP_HEADER_TAG_HAMT_NODE_BITMAP,
    };
    use crate::thing::{
        big_size_words, external_pid_size_words, external_port_size_words,
        external_ref_size_words, write_big, write_external_pid, write_external_port,
        write_external_ref, write_float,
    };
    use crate::tuple::tuple0;
    use crate::AtomTable;

    /// A scratch word the header-only enumerators may point at (a shape that never reads
    /// `ptr` — a tuple, record or fun — does not touch it).
    const SCRATCH: [Eterm; 1] = [Eterm::from_raw(0)];

    /// [`object_words`] for a live boxed term, reading its header word from the term.
    fn words_of(t: Eterm) -> usize {
        let p: *const Eterm = boxed_val(t);
        // SAFETY: `t` is a live boxed term, so `p` addresses its live header word.
        unsafe { object_words(*p, p) }
    }

    /// The **sizing** child offsets of the object at `ptr`.
    fn sizing_slots(header: Eterm, ptr: *const Eterm) -> Vec<usize> {
        let mut out = Vec::new();
        // SAFETY: every caller passes a live header word and its live object.
        unsafe { for_each_sizing_child_slot(header, ptr, |offset| out.push(offset)) };
        out
    }

    /// The **copy** child offsets of the object at `ptr`.
    fn copy_slots(header: Eterm, ptr: *const Eterm) -> Vec<usize> {
        let mut out = Vec::new();
        // SAFETY: every caller passes a live header word and its live object.
        unsafe { for_each_child_slot(header, ptr, |offset| out.push(offset)) };
        out
    }

    // -----------------------------------------------------------------------
    // `object_words` — every shape
    // -----------------------------------------------------------------------

    #[test]
    fn object_words_of_tuples_and_records_is_arity_plus_one() {
        // `make_arityval` requires `size > 0`, so the zero case uses the dedicated
        // zero-arity header (raw word `0`, the shared empty-tuple literal).
        for (arity, hdr) in [
            (0usize, make_arityval_zero()),
            (1, make_arityval(1)),
            (2, make_arityval(2)),
            (3, make_arityval(3)),
            (10, make_arityval(10)),
        ] {
            // SAFETY: `SCRATCH` stands in for the header address; a tuple reads no payload.
            assert_eq!(
                unsafe { object_words(hdr, SCRATCH.as_ptr()) },
                arity + 1,
                "arity {arity}"
            );
        }
        // A native record's *instance* header arity is `field_count + 1`.
        for n in [1usize, 2, 3] {
            let hdr = make_record_header(n + 1);
            assert_eq!(
                // SAFETY: as above — a record reads no payload here.
                unsafe { object_words(hdr, SCRATCH.as_ptr()) },
                record_inst_size_words(n),
                "{n}-field record instance"
            );
        }
    }

    #[test]
    fn object_words_of_funs_covers_all_three_kinds() {
        for (kind, num_free, expected) in [
            (FunKind::Local, 0usize, 1 + 2),
            (FunKind::Local, 3, 1 + 2 + 3),
            (FunKind::External, 0, 1 + 3),
            // No writer builds an external fun with an environment (the C asserts
            // `num_free == 0`), but the layout must still size one consistently from
            // `desc_words(External) == 3` rather than the legacy `ERL_FUN_SIZE`.
            (FunKind::External, 2, 1 + 3 + 2),
            (FunKind::Imported, 0, 1 + 10),
            (FunKind::Imported, 2, 1 + 10 + 2),
        ] {
            let hdr = make_fun_header(1, num_free, kind).expect("a valid fun header");
            assert_eq!(
                // SAFETY: a fun header at a scratch address; a fun reads no payload.
                unsafe { object_words(hdr, SCRATCH.as_ptr()) },
                expected,
                "{kind:?} fun with {num_free} free vars"
            );
            assert_eq!(expected, fun_words(kind, num_free));
        }
    }

    #[test]
    fn object_words_of_a_flatmap_is_size_plus_three() {
        for size in [0usize, 1, 2, 31, 32] {
            // `[thing_word, size, keys, V0..V(size-1)]`.
            let words = [
                make_header(1usize << 2, MAP_SUBTAG),
                Eterm::from_raw(size),
                make_small(0),
                make_small(1),
                make_small(2),
            ];
            assert!(is_flatmap_header(words[0]));
            // SAFETY: `words` outlives the call and heads a live flatmap.
            assert_eq!(
                unsafe { object_words(words[0], words.as_ptr()) },
                size + 3,
                "flatmap of size {size}"
            );
        }
    }

    #[test]
    fn object_words_of_every_hamt_node_kind() {
        // Head-array: `[hdr][size][16 slots]` — 18 words.
        let head_array = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff); 18];
        assert!(is_map_header(head_array[0]) && !is_flatmap_header(head_array[0]));
        // SAFETY: a live head-array node.
        assert_eq!(unsafe { object_words(head_array[0], head_array.as_ptr()) }, 18);

        // Head-bitmap with two set bits: `[hdr][size][slot][slot]` — `2 + nslots`.
        let head_bitmap = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0011); 4];
        // SAFETY: a live head-bitmap node.
        assert_eq!(unsafe { object_words(head_bitmap[0], head_bitmap.as_ptr()) }, 4);

        // Inner node-bitmap with three set bits: `1 + nslots`.
        let inner = [make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b1011); 4];
        // SAFETY: a live inner node-bitmap.
        assert_eq!(unsafe { object_words(inner[0], inner.as_ptr()) }, 4);
    }

    #[test]
    fn object_words_of_leaf_things_is_thing_arity_plus_one() {
        let mut heap = Heap::new(64);

        // Float: `[HEADER_FLONUM, bits]` => 2.
        // SAFETY: 2 words reserved for the flonum.
        let float = unsafe { write_float(heap.alloc_words(2).unwrap(), 1.5) };
        assert_eq!(words_of(float), 2);

        // A 2-digit bignum: `big_size_words(2) == 3`.
        // SAFETY: `big_size_words(2)` words reserved for the bignum.
        let big = unsafe { write_big(heap.alloc_words(big_size_words(2)).unwrap(), false, &[1, 2]) };
        assert_eq!(words_of(big), 3);

        // A heap bitstring: `[thing_word, n_bits, data…]`.
        let bits_words = heap_bits_size_words(64);
        // SAFETY: `bits_words` words reserved for the heap-bits block.
        let bits = unsafe {
            write_heap_bits(heap.alloc_words(bits_words).unwrap(), &[0xAA; 8], 64).unwrap()
        };
        assert_eq!(words_of(bits), bits_words);

        // External pid / port / ref.
        // SAFETY: each block is sized by its own `*_size_words` and all are live.
        let (pid, port, rf) = unsafe {
            let pid = write_external_pid(
                heap.alloc_words(external_pid_size_words()).unwrap(),
                make_small(0),
                1,
                2,
                3,
            );
            let port = write_external_port(
                heap.alloc_words(external_port_size_words()).unwrap(),
                make_small(0),
                4,
                5,
            );
            let rf = write_external_ref(
                heap.alloc_words(external_ref_size_words(2)).unwrap(),
                make_small(0),
                &[6, 7],
                8,
            );
            (pid, port, rf)
        };
        for (term, expected) in [
            (pid, external_pid_size_words()),
            (port, external_port_size_words()),
            (rf, external_ref_size_words(2)),
        ] {
            assert_eq!(words_of(term), expected);
        }

        // An off-heap `BinRef` container `[HEADER_BIN_REF, val, next]` => 3.
        assert_eq!(
            // SAFETY: `HEADER_BIN_REF` is a bare thing header; a `BinRef` reads no `ptr`.
            unsafe { object_words(HEADER_BIN_REF, SCRATCH.as_ptr()) },
            3
        );
    }

    #[test]
    fn object_words_of_sub_bits_covers_the_three_cases() {
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut heap = Heap::new(128);

        // (a) A real refc binary: the whole `[BinRef(3)][ErlSubBits(5)]` pair.
        // SAFETY: 8 words reserved for the pair; `payload` outlives the term.
        let refc = unsafe {
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
        assert_eq!(words_of(refc), ERL_REFC_BITS_SIZE);
        assert_eq!(ERL_REFC_BITS_SIZE, 8);

        // (b) A match context over an on-heap bitstring: the copied window's words.
        // SAFETY: `heap_bits_size_words(64)` words for a 64-bit (8-byte) heap bitstring.
        let base = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(64)).unwrap();
            write_heap_bits(hp, &[0xAA; 8], 64).unwrap()
        };
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
            assert_eq!(words_of(sub), expected, "{bits}-bit match context window");
            assert_eq!(expected, heap_bits_size_words(bits));
        }

        // (c) A standalone/malformed `ErlSubBits` (a non-boxed `orig`): its thing size.
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let standalone = unsafe {
            let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                make_small(3),
            )
        };
        assert_eq!(words_of(standalone), ERL_SUB_BITS_SIZE);
    }

    // -----------------------------------------------------------------------
    // Child-slot enumeration — every shape
    // -----------------------------------------------------------------------

    #[test]
    fn tuple_and_record_child_slots_are_the_elements() {
        for arity in [1usize, 3, 5] {
            let hdr = make_arityval(arity);
            // SAFETY: a tuple header at a scratch address; the shape reads no payload.
            let slots = sizing_slots(hdr, SCRATCH.as_ptr());
            assert_eq!(slots, (1..=arity).collect::<Vec<_>>(), "tuple arity {arity}");
        }
        // A native record instance walks its definition pointer + field values.
        let hdr = make_record_header(4);
        // SAFETY: as above.
        assert_eq!(sizing_slots(hdr, SCRATCH.as_ptr()), vec![1, 2, 3, 4]);
    }

    #[test]
    fn fun_child_slots_are_the_environment_after_the_descriptor() {
        for (kind, num_free, env) in [
            (FunKind::Local, 3usize, 1 + 2usize),
            (FunKind::Imported, 2, 1 + 10),
            (FunKind::External, 0, 0), // no environment at all
            (FunKind::External, 2, 1 + 3), // env starts after the 3-word descriptor
        ] {
            let hdr = make_fun_header(1, num_free, kind).expect("a valid fun header");
            // SAFETY: a fun header at a scratch address; the shape reads no payload.
            let slots = sizing_slots(hdr, SCRATCH.as_ptr());
            let expected: Vec<usize> = if num_free == 0 {
                Vec::new()
            } else {
                (env..env + num_free).collect()
            };
            assert_eq!(slots, expected, "{kind:?} fun");
        }
    }

    #[test]
    fn flatmap_child_slots_are_keys_tuple_and_values() {
        // `[hdr, size=2, keys, V0, V1]` => keys at 2, values at 3 and 4.
        let words = [
            make_header(1usize << 2, MAP_SUBTAG),
            Eterm::from_raw(2),
            make_small(0),
            make_small(1),
            make_small(2),
        ];
        // SAFETY: a live flatmap head in `words`.
        assert_eq!(sizing_slots(words[0], words.as_ptr()), vec![2, 3, 4]);
        // SAFETY: as above — the copy children of a flatmap are the same.
        assert_eq!(copy_slots(words[0], words.as_ptr()), vec![2, 3, 4]);
    }

    #[test]
    fn hamt_child_slots_are_the_occupied_slots() {
        // Head-array: one slot per word 2..18.
        let head_array = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff); 18];
        // SAFETY: a live head-array node.
        assert_eq!(
            sizing_slots(head_array[0], head_array.as_ptr()),
            (2..18).collect::<Vec<_>>()
        );

        // Head-bitmap (bits 0 and 2): slots start after the head's `size` word.
        let head_bitmap = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0101); 4];
        // SAFETY: a live head-bitmap node.
        assert_eq!(sizing_slots(head_bitmap[0], head_bitmap.as_ptr()), vec![2, 3]);

        // Inner node-bitmap (bits 0, 1, 3): no `size` word, slots start at word 1.
        let inner = [make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b1011); 4];
        // SAFETY: a live inner node-bitmap.
        assert_eq!(sizing_slots(inner[0], inner.as_ptr()), vec![1, 2, 3]);
    }

    #[test]
    fn leaf_things_have_no_child_slots() {
        let mut heap = Heap::new(32);
        // SAFETY: 2 words for the flonum.
        let float = unsafe { write_float(heap.alloc_words(2).unwrap(), 2.5) };
        // SAFETY: a live flonum.
        assert!(sizing_slots(unsafe { *boxed_val(float) }, boxed_val(float)).is_empty());
        // SAFETY: a bare thing header at a scratch address; a `BinRef` reads no payload.
        assert!(sizing_slots(HEADER_BIN_REF, SCRATCH.as_ptr()).is_empty());
        // SAFETY: as above.
        assert!(copy_slots(HEADER_FLONUM, SCRATCH.as_ptr()).is_empty());
    }

    #[test]
    fn sub_bits_children_diverge_between_sizing_and_copying() {
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([0u8; 8]);
        let mut heap = Heap::new(64);

        // A refc binary: sizing descends nowhere, copying reports the `orig` word.
        // SAFETY: 8 words reserved for the pair.
        let refc = unsafe {
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
        // SAFETY: a live refc binary; its `orig` word is a live `BinRef` term.
        let sizing = sizing_slots(unsafe { *boxed_val(refc) }, boxed_val(refc));
        assert!(sizing.is_empty(), "sizing must not descend into `orig`");
        // SAFETY: as above.
        let copying = copy_slots(unsafe { *boxed_val(refc) }, boxed_val(refc));
        assert_eq!(copying, vec![SUB_BITS_ORIG_WORD]);
        assert_eq!(SUB_BITS_ORIG_WORD, 4);

        // A standalone `ErlSubBits` (a non-boxed `orig`) behaves the same at slot level.
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let standalone = unsafe {
            let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                make_small(0),
            )
        };
        // SAFETY: a live `ErlSubBits`.
        assert!(sizing_slots(unsafe { *boxed_val(standalone) }, boxed_val(standalone)).is_empty());
        // SAFETY: as above.
        assert_eq!(
            copy_slots(unsafe { *boxed_val(standalone) }, boxed_val(standalone)),
            vec![4]
        );
    }

    #[test]
    fn child_offsets_are_one_contiguous_ascending_run() {
        // The documented invariant callers rely on: the yielded offsets are exactly
        // `first ..= first + count - 1`.
        let head_array = [make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff); 18];
        let flat = [
            make_header(1usize << 2, MAP_SUBTAG),
            Eterm::from_raw(2),
            make_small(0),
            make_small(1),
            make_small(2),
        ];
        for (header, ptr, expected) in [
            (make_arityval(4), SCRATCH.as_ptr(), vec![1, 2, 3, 4]),
            (flat[0], flat.as_ptr(), vec![2, 3, 4]),
            (head_array[0], head_array.as_ptr(), (2..18).collect()),
        ] {
            // SAFETY: each `(header, ptr)` is a live object built above.
            let slots = sizing_slots(header, ptr);
            assert_eq!(slots, expected);
            let contiguous = slots
                .iter()
                .enumerate()
                .all(|(i, &offset)| offset == slots[0] + i);
            assert!(contiguous, "offsets must be one contiguous ascending run");
        }
    }

    #[test]
    fn enumerators_visit_the_built_map_graph() {
        // A real `write_map` hash map: every visited root slot is a non-immediate child
        // and `object_words` matches the node's own size.
        let mut buf = vec![Eterm::from_raw(0); map_words_capacity(40)];
        let atoms = AtomTable::new();
        let pairs: Vec<(Eterm, Eterm)> = (1..=40).map(|k| (make_small(k), make_small(k))).collect();
        // SAFETY: `buf` owns `map_words_capacity(40)` words and outlives `map`.
        let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.unwrap();
        let p = boxed_val(map);
        // SAFETY: `map` is a live boxed map, so `p` heads it.
        let header = unsafe { *p };
        assert!(is_map_header(header) && !is_flatmap_header(header));
        let slots = sizing_slots(header, p);
        assert!(!slots.is_empty());
        for &offset in &slots {
            // SAFETY: `offset` is an in-bounds slot of `map`.
            assert!(!is_immediate(unsafe { *p.add(offset) }));
        }
        assert_eq!(slots.len(), map_header_val(header).count_ones() as usize);
        // SAFETY: a live map head.
        assert_eq!(
            unsafe { object_words(header, p) },
            1 + header_arity(header) + slots.len()
        );
    }

    // -----------------------------------------------------------------------
    // Cross-check: the two halves agree on a whole graph
    // -----------------------------------------------------------------------

    /// Sums [`object_words`] over the whole graph reachable from `root`, descending the
    /// **sizing** child slots — the walk `size_object` performs, built from the
    /// per-object API instead of `size.rs`'s private model.
    ///
    /// `size.rs`'s conventions are reproduced: a cons cell is `2` words, the global
    /// empty-tuple literal (a literal-tagged arity-0 `ARITYVAL`, `copy.c:125-140`)
    /// contributes `0`, and a non-term/immediate contributes nothing. An explicit DFS
    /// keeps a path set, so a shared subterm is counted per occurrence (like
    /// `size_object`) and an accidental cycle terminates.
    fn sum_object_words(root: Eterm) -> usize {
        enum Step {
            Enter(Eterm),
            Exit(usize),
        }

        let mut total = 0usize;
        let mut on_path: HashSet<usize> = HashSet::new();
        let mut steps: Vec<Step> = vec![Step::Enter(root)];
        while let Some(step) = steps.pop() {
            match step {
                Step::Exit(addr) => {
                    on_path.remove(&addr);
                }
                Step::Enter(t) => {
                    if !is_term_word(t) || is_immediate(t) {
                        continue;
                    }
                    if is_list(t) {
                        let cell = list_val(t);
                        if !on_path.insert(cell as usize) {
                            continue;
                        }
                        total += 2;
                        steps.push(Step::Exit(cell as usize));
                        // SAFETY: `t` is a live, non-`NIL` cons cell.
                        let (head, tail) = unsafe { (*cell, *cell.add(1)) };
                        steps.push(Step::Enter(tail));
                        steps.push(Step::Enter(head));
                        continue;
                    }
                    let p = boxed_val(t);
                    // SAFETY: `t` is a live boxed term, so `p` heads it.
                    let header = unsafe { *p };
                    if header.raw() & TAG_HEADER_MASK == ARITYVAL_SUBTAG
                        && header_arity(header) == 0
                        && crate::literal::is_literal(t)
                    {
                        // The global empty-tuple literal (`copy.c:125-140`).
                        continue;
                    }
                    // SAFETY: `header`/`p` head a live object.
                    let has_children =
                        unsafe { sizing_child_slots(header, p) }.is_some();
                    if has_children && !on_path.insert(p as usize) {
                        continue;
                    }
                    // SAFETY: a live boxed object heading a live payload.
                    total += unsafe { object_words(header, p) };
                    if has_children {
                        steps.push(Step::Exit(p as usize));
                        // SAFETY: `header`/`p` head a live object whose children are live.
                        unsafe {
                            for_each_sizing_child_slot(header, p, |offset| {
                                steps.push(Step::Enter(*p.add(offset)));
                            });
                        }
                    }
                }
            }
        }
        total
    }

    /// Checks the two halves agree on `term`: the per-object words sum to `size_object`.
    fn assert_halves_agree(term: Eterm, label: &str) {
        assert_eq!(
            sum_object_words(term),
            size_object(term),
            "per-object words must sum to `size_object` ({label})"
        );
    }

    #[test]
    fn summing_object_words_matches_size_object_for_every_shape() {
        let mut heap = Heap::new(512);

        // Immediates, cons cells, tuples, nested tuples.
        assert_halves_agree(make_small(7), "small int");
        assert_halves_agree(NIL, "nil");
        assert_halves_agree(tuple0(), "empty tuple literal");
        let l3 = {
            let t = heap.cons(make_small(3), NIL).unwrap();
            let t = heap.cons(make_small(2), t).unwrap();
            heap.cons(make_small(1), t).unwrap()
        };
        assert_halves_agree(l3, "3-element list");
        let tup = heap.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
        assert_halves_agree(tup, "tuple");
        let inner = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer = heap.tuple(&[inner, make_small(9)]).unwrap();
        assert_halves_agree(outer, "nested tuple");

        // A local fun closing over a tuple.
        let free = [inner, make_small(0)];
        // SAFETY: `fun_words` sizes the block exactly.
        let local = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::Local, free.len())).unwrap();
            write_fun_local(hp, 1, 1, 0, &free).unwrap()
        };
        assert_halves_agree(local, "local fun");

        // An imported (`NEW_FUN_EXT`) fun with no environment.
        // SAFETY: `fun_words` sizes the 10-descriptor block exactly.
        let imported = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::Imported, 0)).unwrap();
            write_fun_imported(
                hp,
                0,
                crate::Atom::from_raw(1),
                &[0u8; 16],
                1,
                2,
                3,
                FunPid {
                    node: crate::Atom::from_raw(2),
                    number: 4,
                    serial: 5,
                    creation: 6,
                },
                &[],
            )
            .unwrap()
        };
        assert_halves_agree(imported, "imported fun");

        // A native record: the instance descends into its definition + field-order tuple.
        let mut table = AtomTable::new();
        let fields = [
            (make_atom(table.intern("a")), make_small(1)),
            (make_atom(table.intern("b")), make_small(2)),
        ];
        // SAFETY: `record_block_words(2)` sizes the whole construction block.
        let rec = unsafe {
            let hp = heap.alloc_words(record_block_words(2)).unwrap();
            write_record(
                hp,
                table.intern("m"),
                table.intern("r"),
                false,
                &fields,
                make_small(0),
            )
            .unwrap()
        };
        assert_halves_agree(rec, "native record");

        // A flat map (<= 32 keys) and HAMT hash maps (33+ keys).
        for n in [3i64, 33, 60] {
            let pairs: Vec<(Eterm, Eterm)> =
                (1..=n).map(|k| (make_small(k), make_small(k * 7))).collect();
            let atoms = AtomTable::new();
            let mut buf = vec![Eterm::from_raw(0); map_words_capacity(pairs.len())];
            // SAFETY: `buf` owns the words and outlives `map`.
            let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.unwrap();
            assert_halves_agree(map, &format!("{n}-key map"));
        }

        // A refc binary and a match context.
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([9u8; 8]);
        // SAFETY: 8 words reserved for the pair; `payload` outlives the term.
        let refc = unsafe {
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
        assert_halves_agree(refc, "refc binary");
        // Reached through a cons cell, the pair is still counted once.
        let cell = heap.cons(refc, NIL).unwrap();
        assert_halves_agree(cell, "cons of a refc binary");

        // SAFETY: `heap_bits_size_words(64)` words for a 64-bit heap bitstring.
        let base = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(64)).unwrap();
            write_heap_bits(hp, &[0xAA; 8], 64).unwrap()
        };
        // SAFETY: 5 words for the `ErlSubBits`.
        let sub = unsafe {
            let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(520),
                base,
            )
        };
        assert_halves_agree(sub, "match context");

        // A leaf thing reached through a tuple.
        // SAFETY: 2 words for the flonum.
        let float = unsafe { write_float(heap.alloc_words(2).unwrap(), 3.25) };
        let leaf_tuple = heap.tuple(&[float, make_small(4)]).unwrap();
        assert_halves_agree(leaf_tuple, "tuple of a float");
    }
}
