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

//! The **complete header-space classification table**: `size_object` vs
//! `layout::object_words`, one row per header shape.
//!
//! Two entry points answer "how many heap words does this object occupy?":
//!
//! * [`beam_term::layout::object_words`] — the per-object interior table every copier
//!   writes its destination with (`beam_message::shallow`, `beam_gc::copy`), taken
//!   straight from the `size_object_x` switch (`copy.c:88-263`);
//! * [`beam_term::size::size_object`] — the graph walk that prices a whole term
//!   (`size_object_x(t, NULL)`), which must charge each object exactly what the copier
//!   reserves.
//!
//! They must agree object for object, because an **under**-count is the dangerous
//! direction: a copier would reserve too little and refuse (or corrupt) a copy that
//! should have succeeded. One shape is deliberately different — the global empty-tuple
//! literal (`copy.c:125-140`) — and this file pins that too.
//!
//! The sweep covers the **whole header space**: all 14 [`BoxedTag`] subtags plus the two
//! reserved subtag values the enum deliberately omits (`0x04`, `0x3C` —
//! `BoxedTag::from_raw` returns `None` for them, as the C `tag_val_def` aborts on them),
//! each over the `0/1/3` arity sweep, or over the shape's own interior where the subtag
//! owns it (flatmap head vs HAMT heads, the three fun kinds, the three `ErlSubBits`
//! cases). Per row it asserts:
//!
//! 1. `layout::object_words(header, ptr)` equals the hand-derived count;
//! 2. `size_object(term)` equals the hand-derived count (the graph total);
//! 3. `size_object_bounded(term, …)` is `Ok(the same)`, tight at the exact budget and
//!    refusing one word below it;
//! 4. the policy seam is **unobservable**: `size_object_with(…, &DefaultSizingPolicy)`
//!    and `size_object_bounded_with(…, &DefaultSizingPolicy)` reproduce both;
//! 5. `for_each_sizing_child_slot` yields the shape's own slot run — ascending, inside
//!    the object, and (for a synthetic zeroed payload) holding the non-term
//!    `Eterm::from_raw(0)`, so descending cannot change the count: rows 1 and 2 are
//!    statements about the two *pricing* rules and nothing else. `for_each_child_slot`
//!    differs in exactly one place, the `ErlSubBits` `orig` word, pinned explicitly.
//!
//! The last test repeats the table's numbers on objects built with the **real
//! constructors** (`record::write_record`, `thing::write_flatmap`, `refc::write_refc_binary`,
//! `bitstring::write_sub_bits`, `map::write_map`), so the table is shown to describe real
//! objects and not only synthetic headers.

use beam_term::prelude::*;

/// Every header subtag value, ascending: the 14 [`BoxedTag`] variants plus the two
/// reserved values (`0x04`, `0x3C`) the enum omits. Pinned against the enum by
/// [`the_sweep_covers_the_whole_header_space`].
const SUBTAGS: [usize; 16] = [
    0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x1C, 0x20, 0x24, 0x28, 0x2C, 0x30, 0x34, 0x38, 0x3C,
];

/// One row of the table: a header shape, the payload that header heads, and the word
/// count ground truth (`copy.c:88-263` and the per-shape table in `layout.rs`).
struct Row {
    /// The shape, for assertion messages.
    label: String,
    /// The boxed root; its header word is the row's header.
    term: Eterm,
    /// Words [`object_words`] reports for the root object, header word included.
    object_words: usize,
    /// Words [`size_object`] reports for the whole graph rooted at `term`.
    size_object: usize,
    /// The number of **sizing** child slots the root exposes.
    sizing_slots: usize,
    /// Every payload word is the non-term `Eterm::from_raw(0)`, so no child can
    /// contribute a word: the row prices the *object* and nothing else.
    zeroed_payload: bool,
    /// `for_each_child_slot` reports one extra slot (the `ErlSubBits` `orig`) — the one
    /// documented sizing/copying divergence.
    copy_slots_extra: bool,
}

impl Row {
    fn check(&self) {
        let p = boxed_val(self.term);
        // SAFETY: every row's `term` is built from heap words the caller's arena owns and
        // outlives this check, and `p` is that block's header word (`boxed_val`).
        let header = unsafe { *p };
        let label = &self.label;

        // (1) The per-object interior table (`layout.rs`).
        // SAFETY: `p` addresses the live header word of a live object of `header`'s shape.
        let words = unsafe { object_words(header, p) };
        assert_eq!(words, self.object_words, "{label}: layout::object_words");

        // (2)+(3) The graph walk, bounded and unbounded.
        assert_eq!(
            size_object(self.term),
            self.size_object,
            "{label}: size_object"
        );
        assert_eq!(
            size_object_bounded(self.term, usize::MAX),
            Ok(self.size_object),
            "{label}: size_object_bounded(usize::MAX)"
        );
        assert_eq!(
            size_object_bounded(self.term, self.size_object),
            Ok(self.size_object),
            "{label}: the exact budget fits"
        );
        if self.size_object > 0 {
            assert_eq!(
                size_object_bounded(self.term, self.size_object - 1),
                Err(TermError::TermGraphTooLarge),
                "{label}: one word below the total must refuse"
            );
        }

        // (4) The policy seam is unobservable for every shape.
        assert_eq!(
            size_object_with(self.term, &DefaultSizingPolicy),
            self.size_object,
            "{label}: size_object_with(&DefaultSizingPolicy)"
        );
        assert_eq!(
            size_object_bounded_with(self.term, usize::MAX, &DefaultSizingPolicy),
            Ok(self.size_object),
            "{label}: size_object_bounded_with(&DefaultSizingPolicy)"
        );

        // (5) The child enumerations.
        let mut sizing: Vec<usize> = Vec::new();
        // SAFETY: as above — a live header word and its live payload words.
        unsafe { for_each_sizing_child_slot(header, p, |offset| sizing.push(offset)) };
        assert_eq!(sizing.len(), self.sizing_slots, "{label}: sizing child slots");
        if let Some(&first) = sizing.first() {
            for (i, &offset) in sizing.iter().enumerate() {
                assert_eq!(
                    offset,
                    first + i,
                    "{label}: one object's slots form one ascending contiguous run"
                );
                assert!(
                    offset < self.object_words,
                    "{label}: slot {offset} lies inside the {}-word object",
                    self.object_words
                );
                if self.zeroed_payload {
                    // SAFETY: `offset` is in bounds by the assertion above.
                    assert_eq!(
                        unsafe { *p.add(offset) },
                        Eterm::from_raw(0),
                        "{label}: slot {offset} is the non-term zero word, so a child \
                         contributes no words"
                    );
                }
            }
        }
        let mut copying: Vec<usize> = Vec::new();
        // SAFETY: as above.
        unsafe { for_each_child_slot(header, p, |offset| copying.push(offset)) };
        if self.copy_slots_extra {
            assert_eq!(
                copying,
                vec![SUB_BITS_ORIG_WORD],
                "{label}: the copier forwards exactly the `orig` word"
            );
            assert!(
                sizing.is_empty(),
                "{label}: ... and the sizer exposes no child at all"
            );
        } else {
            assert_eq!(
                copying, sizing,
                "{label}: the two enumerations agree outside `ErlSubBits`"
            );
        }
    }
}

/// Allocates a `1 + payload.len()`-word block whose word 0 is `header` and whose
/// payload words are `payload`, and returns the boxed root.
fn block(heap: &mut Heap, header: Eterm, payload: &[Eterm]) -> Eterm {
    // SAFETY: the arena hands out `1 + payload.len()` uninitialised words and every one of
    // them is written before the pointer escapes (the arena never reallocates).
    unsafe {
        let p = heap.alloc_words(1 + payload.len()).expect("the block fits");
        *p = header;
        for (i, word) in payload.iter().enumerate() {
            *p.add(1 + i) = *word;
        }
        make_boxed(p as *const Eterm)
    }
}

/// [`block`] with every payload word the **non-term** `Eterm::from_raw(0)` (a bare header
/// word, `tag::is_term_word` false), so the walk never visits a child.
fn zeroed(heap: &mut Heap, header: Eterm, payload_len: usize) -> Eterm {
    block(heap, header, &vec![Eterm::from_raw(0); payload_len])
}

/// The table's expectation for the generic header `make_header(arity, subtag)`: the
/// object's words and its sizing child-slot count.
fn generic_expectation(subtag: usize, arity: usize) -> (usize, usize) {
    match subtag {
        // A fun header's arity field is the C `ERL_FUN_SIZE - 1` while its `num_free` and
        // kind bits live above this header's arity field, so this is a 0-env local fun:
        // `1 + desc_words(Local) + 0` (`layout.rs`, `crate::fun`).
        s if s == FUN_SUBTAG => (fun_words(FunKind::Local, 0), 0),
        // `sub_bits_words` reads `start`/`end`/`orig` (a non-boxed `orig` here) and never
        // the header arity: the standalone thing size (`copy.c:241`).
        s if s == SUB_BITS_SUBTAG => (ERL_SUB_BITS_SIZE, 0),
        // A map header owns its own interior: bits 6..7 are the 2-bit map tag, bits 8..15
        // `MAP_HEADER_ARITY` and bits 16..31 `MAP_HEADER_VAL` — all zero for this header.
        // An arity whose low two bits are 0 is the flatmap head (`size` word 0 => 3 words,
        // one child = the keys tuple); every other tag is a HAMT node with no set
        // occupancy bit.
        s if s == MAP_SUBTAG => {
            if arity & 0x3 == 0 {
                (3, 1)
            } else {
                (1, 0)
            }
        }
        // A tuple and a native record are both transparent: `arity + 1` words, with the
        // `arity` element words as children (`copy.c:124-140`, `layout.rs`).
        s if s == ARITYVAL_SUBTAG || s == RECORD_SUBTAG => (arity + 1, arity),
        // Everything else — bignum, ref, float, heap bitstring, `BinRef`, external
        // pid/port/ref, and the two reserved subtags — is the `thing_arityval + 1` (i.e.
        // `arity + 1`) arm with no child slot.
        _ => (arity + 1, 0),
    }
}

/// The generic row for `(subtag, arity)`.
///
/// The payload is at least 4 words even when the header arity is smaller, because two
/// shapes address words their header arity does not cover: the flatmap head's `size` word
/// (word 1) and an `ErlSubBits`'s `start`/`end`/`orig` (words 2..4).
fn generic_row(heap: &mut Heap, subtag: usize, arity: usize) -> Row {
    let (object_words, sizing_slots) = generic_expectation(subtag, arity);
    Row {
        label: format!("subtag 0x{subtag:02X} at arity {arity}"),
        term: zeroed(heap, make_header(arity, subtag), arity.max(4)),
        object_words,
        size_object: object_words,
        sizing_slots,
        zeroed_payload: true,
        copy_slots_extra: subtag == SUB_BITS_SUBTAG,
    }
}

/// The sweep really is the whole header space: 16 values, the 14 [`BoxedTag`] variants
/// plus exactly the two reserved subtags.
#[test]
fn the_sweep_covers_the_whole_header_space() {
    assert_eq!(SUBTAGS.len(), 16, "a 6-bit subtag has 16 values at a 4-bit stride");
    for window in SUBTAGS.windows(2) {
        assert_eq!(window[1], window[0] + 4, "the sweep is contiguous");
    }
    let named: Vec<usize> = SUBTAGS
        .iter()
        .copied()
        .filter(|v| BoxedTag::from_raw(*v as u32).is_some())
        .collect();
    let reserved: Vec<usize> = SUBTAGS
        .iter()
        .copied()
        .filter(|v| BoxedTag::from_raw(*v as u32).is_none())
        .collect();
    assert_eq!(named.len(), 14, "every `BoxedTag` variant is covered");
    assert_eq!(reserved, vec![0x04, 0x3C], "the two reserved subtag values");
}

/// The generic sweep: `make_header(arity, subtag)` for every subtag at arity 0, 1 and 3.
#[test]
fn every_subtag_agrees_over_the_arity_sweep() {
    let mut heap = Heap::new(4096);
    for subtag in SUBTAGS {
        for arity in [0usize, 1, 3] {
            generic_row(&mut heap, subtag, arity).check();
        }
    }
}

/// The rows the sweep cannot express, each named with its disposition.
#[test]
fn the_named_exception_rows() {
    let mut heap = Heap::new(64);

    // (a) The global empty-tuple literal — the ONE deliberate divergence, and the only
    // shape in the whole header space where the two entry points differ.
    // `size_object` prices it `0`: it is `ERTS_GLOBAL_LIT_EMPTY_TUPLE`, one process-wide
    // constant a copy never materialises (`copy.c:130-140`, "The empty tuple is always a
    // global literal constant so it does not take up any extra space"), and the oracle
    // agrees (`erts_debug:flat_size({}) == 0`). `object_words` reports the 1-word block it
    // really is, which is why every caller that must reproduce `size_object` skips this
    // literal *before* calling it.
    Row {
        label: "tuple0() — the literal-tagged arity-0 ARITYVAL".into(),
        term: tuple0(),
        object_words: 1,
        size_object: 0,
        sizing_slots: 0,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();

    // (b) An UNTAGGED, hand-built arity-0 tuple block. The C `ASSERT`s this cannot exist
    // (`copy.c:129-131`), so there is no oracle answer to compare against; structurally
    // it is one word, and pricing it one word is the answer that cannot under-reserve.
    Row {
        label: "untagged arity-0 ARITYVAL".into(),
        term: zeroed(&mut heap, make_arityval_zero(), 0),
        object_words: 1,
        size_object: 1,
        sizing_slots: 0,
        zeroed_payload: true,
        copy_slots_extra: false,
    }
    .check();

    // (c) A zero-arity *RECORD* header is **not** the empty tuple — `copy.c:128` asserts
    // `arity > 0 || !is_record_header(hdr)`, and every record instance is
    // `make_record_header(n + 1)` (`crate::record`), so arity >= 1. The public
    // `make_record_header(0)` builder can forge the shape; both entry points must price
    // its structural `arity + 1 == 1`, never the empty-tuple `0`.
    Row {
        label: "zero-arity RECORD header (forged)".into(),
        term: zeroed(&mut heap, make_record_header(0), 0),
        object_words: 1,
        size_object: 1,
        sizing_slots: 0,
        zeroed_payload: true,
        copy_slots_extra: false,
    }
    .check();
}

/// The subtags whose interior is their own, over the shapes they really take.
#[test]
fn the_shape_specific_rows() {
    let mut heap = Heap::new(256);

    // Fun: `1 + desc_words(kind) + num_free`, `env[i]` at `1 + desc_words(kind) + i`.
    for (kind, num_free, object_words) in [
        (FunKind::Local, 0usize, 3usize),
        (FunKind::Local, 2, 5),
        (FunKind::External, 0, 4),
        (FunKind::Imported, 0, 11),
    ] {
        let header = make_fun_header(1, num_free, kind).expect("a valid fun header");
        Row {
            label: format!("{kind:?} fun with {num_free} free variables"),
            term: zeroed(&mut heap, header, fun_words(kind, num_free) - 1),
            object_words,
            size_object: object_words,
            sizing_slots: num_free,
            zeroed_payload: true,
            copy_slots_extra: false,
        }
        .check();
    }

    // Map, flatmap head: `size + 3` words; word 1 is the raw pair count and word 2 the
    // keys tuple (here the non-term zero, so the child contributes nothing).
    Row {
        label: "flatmap head with size 0".into(),
        term: zeroed(&mut heap, make_header(4, MAP_SUBTAG), 2),
        object_words: 3,
        size_object: 3,
        sizing_slots: 1,
        zeroed_payload: true,
        copy_slots_extra: false,
    }
    .check();

    // Map, HAMT nodes: `1 + MAP_HEADER_ARITY + popcount(MAP_HEADER_VAL)`.
    for (tag, arity, val, payload_len, object_words, sizing_slots) in [
        // `[hdr][size][16 slots]`.
        (
            MAP_HEADER_TAG_HAMT_HEAD_ARRAY,
            1usize,
            0xffffusize,
            17usize,
            18usize,
            16usize,
        ),
        // `[hdr][size][slot][slot]`.
        (MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0011, 3, 4, 2),
        // `[hdr][slot][slot][slot]` — an inner node carries no `size` word.
        (MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b1011, 3, 4, 3),
        // An occupancy-empty inner node: still one header word.
        (MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0, 0, 1, 0),
    ] {
        Row {
            label: format!("HAMT node tag {tag} arity {arity} val {val:#06b}"),
            term: zeroed(&mut heap, make_map_header(tag, arity, val), payload_len),
            object_words,
            size_object: object_words,
            sizing_slots,
            zeroed_payload: true,
            copy_slots_extra: false,
        }
        .check();
    }

    // `ErlSubBits` with a non-boxed `orig`: not a term the C accepts (`copy.c:222` would
    // dereference a bogus pointer), so it reports its thing size and exposes no child.
    // SAFETY: 5 words reserved for the `ErlSubBits`; nothing else is read.
    let standalone = unsafe {
        let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).expect("the block fits");
        write_sub_bits(
            hp,
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(8),
            make_small(3),
        )
    };
    Row {
        label: "ErlSubBits with a non-boxed orig".into(),
        term: standalone,
        object_words: ERL_SUB_BITS_SIZE,
        size_object: ERL_SUB_BITS_SIZE,
        sizing_slots: 0,
        zeroed_payload: false,
        copy_slots_extra: true,
    }
    .check();
}

/// The table's numbers, reproduced by objects built with the **real constructors**.
#[test]
fn real_objects_reproduce_the_table() {
    let mut heap = Heap::new(4096);
    let mut table = AtomTable::new();

    // A 0-field native record: a 2-word instance (`make_record_header(1)`) over a 6-word
    // definition whose `field_order` is the shared `tuple0()` literal, so the graph is
    // `2 + 6` words (`crate::record`, `erl_record.h:73-86`).
    // SAFETY: `record_block_words(0)` sizes the whole construction block.
    let zero_field = unsafe {
        let hp = heap
            .alloc_words(record_block_words(0))
            .expect("the record fits");
        write_record(
            hp,
            table.intern("m"),
            table.intern("r"),
            false,
            &[],
            make_small(0),
        )
        .expect("a valid 0-field record")
    };
    Row {
        label: "write_record with 0 fields".into(),
        term: zero_field,
        object_words: record_inst_size_words(0),
        size_object: record_inst_size_words(0) + record_def_size_words(0),
        sizing_slots: 1,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();

    // A 2-field native record: `4` instance words over an 8-word definition and a 3-word
    // `field_order` tuple.
    let fields = [
        (make_atom(table.intern("a")), make_small(1)),
        (make_atom(table.intern("b")), make_small(2)),
    ];
    // SAFETY: `record_block_words(2)` sizes the whole construction block.
    let two_field = unsafe {
        let hp = heap
            .alloc_words(record_block_words(2))
            .expect("the record fits");
        write_record(
            hp,
            table.intern("m"),
            table.intern("r"),
            false,
            &fields,
            make_small(0),
        )
        .expect("a valid 2-field record")
    };
    Row {
        label: "write_record with 2 fields".into(),
        term: two_field,
        object_words: record_inst_size_words(2),
        size_object: record_inst_size_words(2)
            + record_def_size_words(2)
            + (fields.len() + 1),
        // The definition plus the two field values follow the 3-arity instance header.
        sizing_slots: record_inst_size_words(2) - 1,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();

    // A 0-pair flat map: the special 3-word `[head, 0, tuple0()]` block, whose keys tuple
    // is the shared literal and therefore free.
    // SAFETY: `flatmap_size_words(0)` words reserved for the empty block.
    let empty_map = unsafe {
        let hp = heap
            .alloc_words(flatmap_size_words(0))
            .expect("the flatmap fits");
        write_flatmap(hp, &[])
    };
    Row {
        label: "write_flatmap with 0 pairs".into(),
        term: empty_map,
        object_words: 3,
        size_object: 3,
        sizing_slots: 1,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();

    // A 2-pair flat map: a 5-word head over a 3-word keys tuple.
    let pairs = [(make_small(1), make_small(7)), (make_small(2), make_small(9))];
    // SAFETY: `flatmap_size_words(2)` words reserved for the block.
    let small_map = unsafe {
        let hp = heap
            .alloc_words(flatmap_size_words(pairs.len()))
            .expect("the flatmap fits");
        write_flatmap(hp, &pairs)
    };
    Row {
        label: "write_flatmap with 2 pairs".into(),
        term: small_map,
        object_words: 5,
        size_object: 5 + 3,
        sizing_slots: 3,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();

    // A real refc binary: the whole 8-word `[BinRef(3)][ErlSubBits(5)]` pair, counted in
    // one go (`copy.c:224-231`). The oracle agrees: `erts_debug:flat_size(binary:copy(<<0>>, 100)) == 8`.
    #[repr(align(8))]
    struct Payload([u8; 8]);
    let payload = Payload([1, 2, 3, 4, 5, 6, 7, 8]);
    // SAFETY: 8 words reserved for the pair; `payload` outlives the term.
    let refc = unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("the pair fits");
        write_refc_binary(
            hp,
            payload.0.as_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.0.as_ptr(),
            0,
            64,
        )
    };
    Row {
        label: "write_refc_binary (64-bit payload)".into(),
        term: refc,
        object_words: ERL_REFC_BITS_SIZE,
        size_object: ERL_REFC_BITS_SIZE,
        sizing_slots: 0,
        zeroed_payload: false,
        copy_slots_extra: true,
    }
    .check();

    // A match context over an on-heap bitstring: the copy materialises a fresh heap-bits
    // block for the window instead of the `ErlSubBits` (`copy.c:232-239`).
    // SAFETY: `heap_bits_size_words(64)` words for a 64-bit heap bitstring.
    let base = unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(64))
            .expect("the bitstring fits");
        write_heap_bits(hp, &[0xAA; 8], 64).expect("a valid bitstring")
    };
    for (bits, window) in [(9usize, 3usize), (64, 3), (65, 4), (520, 11)] {
        assert_eq!(window, heap_bits_size_words(bits));
        // SAFETY: 5 words for the `ErlSubBits`; `base` is a live heap bitstring.
        let sub = unsafe {
            let hp = heap
                .alloc_words(ERL_SUB_BITS_SIZE)
                .expect("the match context fits");
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(bits),
                base,
            )
        };
        Row {
            label: format!("match context over a {bits}-bit heap bitstring"),
            term: sub,
            object_words: window,
            size_object: window,
            sizing_slots: 0,
            zeroed_payload: false,
            copy_slots_extra: true,
        }
        .check();
    }

    // A 33-key hash map — the smallest HAMT (`MAP_SMALL_MAP_LIMIT == 32`). Its trie for
    // these keys is a 15-word head bitmap `[hdr(bmp)][size][13 slots]` (`MAP_HEADER_VAL`
    // `0x7f7d`) over 11 further nodes totalling 42 node words and 33 two-word leaves:
    // `15 + 42 + 66 == 123`. The head exposes its 13 occupied slots.
    let big: Vec<(Eterm, Eterm)> = (1..=33i64)
        .map(|k| (make_small(k), make_small(k * 7)))
        .collect();
    let atoms = AtomTable::new();
    let mut buf = vec![Eterm::from_raw(0); map_words_capacity(big.len())];
    // SAFETY: `buf` owns the words and outlives the term.
    let hamt = unsafe { write_map(buf.as_mut_ptr(), &big, &atoms) }.expect("a valid hash map");
    Row {
        label: "write_map with 33 keys (HAMT)".into(),
        term: hamt,
        object_words: 15,
        size_object: 123,
        sizing_slots: 13,
        zeroed_payload: false,
        copy_slots_extra: false,
    }
    .check();
}
