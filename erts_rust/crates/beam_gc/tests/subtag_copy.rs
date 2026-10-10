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

//! Copy-correctness evidence for the remaining **boxed/heap term shapes** through a
//! collection — the ones `copy_correctness.rs`, `map_copy.rs`, `fun_copy.rs` and
//! `refc_binary_copy.rs` do not directly exercise:
//!
//! 1. a **bignum**, in both sign forms (the sign lives in the header *subtag*, so a
//!    copier that re-tagged or truncated the header word would flip it rather than
//!    fail loudly);
//! 2. an OTP-29 **native record instance** (the `RECORD_SUBTAG` representation: an
//!    instance → definition → `field_order` tuple chain inside one construction block);
//! 3. an **`ErlSubBits` match context over an on-heap bitstring** — the one shape where
//!    `beam_gc::copy::move_size` deliberately *overrides*
//!    `beam_term::layout::object_words` (the block copies as `ERL_SUB_BITS_SIZE` = 5
//!    words while its `orig` word forwards the underlying `ErlHeapBits` block);
//! 4. the **external port** and **external reference** leaf things;
//! 5. a **HAMT head-array node** (`MAP_HEADER_TAG_HAMT_HEAD_ARRAY`, the `[hdr][size][16
//!    slots]` root `write_map` emits once its root occupancy bitmap is full).
//!
//! Each test builds one shape in a real [`HeapSpace`] beside deliberate garbage, collects
//! the shape as a root, and then asserts **from the updated root** that (a) the root moved
//! into the to-space, (b) every pointer child slot the shape exposes was forwarded, (c) the
//! payload survived, and (d) the to-space holds *exactly* the shape's independently-known
//! footprint — so a copy that sized the object wrongly (one word short or long) cannot pass.
//!
//! Where a formula would be circular the exact footprint is measured **independently of
//! every size heuristic**: the head-array test uses the `MARKER` pre-fill + scan-back
//! technique of `map_copy.rs`, and the match-context test pins the 5-word override
//! structurally (the forwarded `orig` copy must land exactly `ERL_SUB_BITS_SIZE` words
//! behind the `ErlSubBits` copy, which only holds if the copier laid down 5 words).
//!
//! These tests exercise the public `beam_term`/`beam_alloc`/`beam_gc` APIs only; every
//! `unsafe` block carries a `// SAFETY:` rationale and only touches words this test
//! allocated itself (or that the copier just wrote into the to-space).

use beam_alloc::default_allocator;
use beam_gc::prelude::{collect, GcStats, HeapSpace};
use beam_term::prelude::{
    arityval, big_parts, big_size_words, bitstring_parts, boxed_val, erts_map_hash,
    external_port_parts, external_port_size_words, external_ref_parts, external_ref_size_words,
    heap_bits_size_words, is_bignum_header, is_bitstring, is_bitstring_header,
    is_external_port_header, is_external_ref_header, is_flatmap_header, is_map_header,
    is_record_header, make_arityval, make_atom, make_map_header, make_record_header, make_small,
    map_get, map_header_type, map_header_val, map_pairs, map_size, map_words_capacity, object_words,
    record_block_words, record_declaration_keys, record_definition, record_definition_field_order,
    record_definition_hash, record_definition_is_exported, record_definition_keys,
    record_definition_module, record_definition_name, record_field_count, record_get, record_values,
    signed_value, size_object, sub_bits_parts, write_big, write_external_port, write_external_ref,
    write_heap_bits, write_map, write_record, write_sub_bits, AtomTable, Eterm, AM_TRUE,
    BIG_SIGN_BIT, ERL_HEAP_BITS_SIZE, ERL_SUB_BITS_FLAG_MASK, ERL_SUB_BITS_FLAG_MUTABLE,
    ERL_SUB_BITS_SIZE, HAMT_SUBTAG_HEAD_ARRAY, HEADER_SUB_BITS, MAP_HEADER_TAG_HAMT_HEAD_ARRAY,
    MAP_SMALL_MAP_LIMIT,
};

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// A sentinel word written into a reserved-but-unwritten region so a term's *actual*
/// contiguous footprint can be recovered by scanning back over the words the writer did
/// not touch. Its 6-bit subtag is the reserved `0x3C` (no map/node/cons/immediate word the
/// writers produce has that subtag), so it never collides with real data — making the
/// measurement independent of every size heuristic in the codebase.
const MARKER: usize = 0xDEAD_BEEF_DEAD_BE3C;

/// The shared "collect a lone root beside 3 words of garbage" tail.
///
/// Asserts the root was relocated into the to-space and out of the from-space, that the
/// to-space holds exactly `live_words`, and that the only reclaimed words are the 3-word
/// `{7,8}` garbage tuple each caller writes. Returns the relocated root and the statistics
/// so a caller can assert its shape's exact object count too.
fn collect_lone_root(
    from: &HeapSpace,
    to: &mut HeapSpace,
    root: Eterm,
    live_words: usize,
) -> (Eterm, GcStats) {
    let mut roots = [root];
    let stats = collect(from, to, &mut roots).expect("collect");

    let copied = roots[0];
    assert!(
        to.contains_term(copied),
        "the root must point into the to-space"
    );
    assert!(
        !from.contains_term(copied),
        "the root must no longer point into the from-space"
    );
    assert_eq!(
        to.used_words(),
        live_words,
        "the to-space must hold exactly the live {live_words} words"
    );
    assert_eq!(stats.words_copied, live_words);
    assert_eq!(stats.words_reclaimed, 3, "only the {{7,8}} garbage tuple");
    (copied, stats)
}

// ---------------------------------------------------------------------------
// 1. Bignum (both sign forms)
// ---------------------------------------------------------------------------

/// A **bignum** is a leaf thing sized `big_size_words(n) == n + 1`; a collection must
/// relocate it as a leaf and keep the header's sign bit and every `ErtsDigit` word.
///
/// Both sign forms are covered because the sign is part of the header's **subtag**
/// (`POS_BIG_SUBTAG` vs `NEG_BIG_SUBTAG`, i.e. `_BIG_SIGN_BIT`), so a copier that re-tagged
/// the header word would flip the sign rather than fail loudly.
#[test]
fn bignums_of_both_signs_are_copied_as_exactly_sized_leaves() {
    // Two `ErtsDigit`s (least significant first), with a non-zero top limb.
    let digits = [0x0123_4567_89AB_CDEFu64, 0xFEDC_BA98_7654_3210u64];
    let words = big_size_words(digits.len());
    assert_eq!(words, 3, "BIG_NEED_SIZE(2) == 3");

    for negative in [false, true] {
        // A *fresh* from-space per sign: the copier leaves a forwarding marker in the
        // source header, so a collected from-space can never be reused. The three extra
        // words hold the deliberate garbage tuple below.
        let mut from = space(words + 3);
        // SAFETY: exactly `words == big_size_words(digits.len())` exclusively-owned words
        // are reserved at `hp`; `write_big` writes all of them, `digits` is non-empty and
        // the block stays live in `from`.
        let big = unsafe {
            let hp = from.heap_mut().alloc_words(words).expect("the bignum block");
            write_big(hp, negative, &digits)
        };
        assert_eq!(from.used_words(), words);
        // SAFETY: `big` is the live bignum just written.
        unsafe { assert!(is_bignum_header(*boxed_val(big)), "negative = {negative}") };

        // Deliberate garbage: an unreferenced `{7,8}` tuple (3 words).
        let _garbage = from
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("garbage tuple");

        // A to-space sized *exactly* to the block: a wrong size cannot fit.
        let mut to = space(words);
        let (copied, stats) = collect_lone_root(&from, &mut to, big, words);
        assert_eq!(stats.objects_copied, 1, "a bignum is a single leaf object");

        // The sign and every limb word round-trip.
        // SAFETY: `copied` is the live, copied bignum.
        let (sign, limbs) = unsafe { big_parts(copied) };
        assert_eq!(sign, negative, "the sign bit must survive the copy");
        assert_eq!(
            limbs,
            &digits[..],
            "every `ErtsDigit` word must survive in place"
        );

        // SAFETY: `copied` is the live, copied bignum, so word 0 is its header.
        let hdr = unsafe { *boxed_val(copied) };
        assert!(is_bignum_header(hdr));
        assert_eq!(hdr.raw() & BIG_SIGN_BIT != 0, negative);
        assert_eq!(
            arityval(hdr),
            digits.len(),
            "the header arity is the digit count, so the header survived too"
        );
        assert_eq!(
            size_object(copied),
            words,
            "the flat size is the block itself (no child slots)"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Native record instance (OTP-29 `RECORD_SUBTAG`)
// ---------------------------------------------------------------------------

/// An **OTP-29 native record instance** is the `RECORD_SUBTAG` block
/// `[header][definition][field values…]`, whose definition word names a
/// `[field_order tuple][hash][module][name][is_exported][sorted keys…]` arityval. A
/// collection must forward the instance's definition + every field value, then the
/// definition's `field_order` tuple and every field *key*, and size the whole chain exactly.
///
/// The keys are interned in an order that differs from the declaration order, so the
/// definition's `field_order` is the non-identity permutation `[2, 0, 1]`; the copy must
/// preserve both the stored (sorted-key) order and that permutation.
#[test]
fn native_record_instance_is_copied_with_every_field_relocated() {
    const FIELDS: usize = 3;
    /// The words each boxed field value (a 2-element tuple) occupies.
    const VALUE_WORDS: usize = 3;

    // Atom indices decide the definition's sorted-key order; intern them ascending so
    // `alpha < beta < gamma` while the fields below are *declared* gamma, alpha, beta.
    let mut atoms = AtomTable::new();
    let alpha = atoms.intern("alpha");
    let beta = atoms.intern("beta");
    let gamma = atoms.intern("gamma");
    let module = atoms.intern("my_mod");
    let name = atoms.intern("my_rec");

    let mut from = space(record_block_words(FIELDS) + FIELDS * VALUE_WORDS + 3);
    // Three boxed field values with distinguishable contents: value `i` is `{i, 10*i}`.
    let values: Vec<Eterm> = (0..FIELDS as i64)
        .map(|i| {
            from.heap_mut()
                .tuple(&[make_small(i), make_small(i * 10)])
                .expect("value tuple")
        })
        .collect();
    assert_eq!(from.used_words(), FIELDS * VALUE_WORDS);

    // Declaration order (gamma, alpha, beta) — deliberately NOT the sorted-key order.
    let fields = [
        (make_atom(gamma), values[0]),
        (make_atom(alpha), values[1]),
        (make_atom(beta), values[2]),
    ];
    // SAFETY: `record_block_words(FIELDS)` exclusively-owned words are reserved at `hp`;
    // `write_record` writes all of them and every field value is a live term in `from`.
    let rec = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(record_block_words(FIELDS))
            .expect("record block");
        write_record(hp, module, name, true, &fields, make_small(0x1234)).expect("record")
    };
    let live_words = record_block_words(FIELDS) + FIELDS * VALUE_WORDS;
    assert_eq!(from.used_words(), live_words);
    // SAFETY: `rec` is the live record instance just written.
    unsafe { assert!(is_record_header(*boxed_val(rec))) };

    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");

    // A to-space sized exactly to the construction block plus the value tuples.
    let mut to = space(live_words);
    let (copied, stats) = collect_lone_root(&from, &mut to, rec, live_words);
    // The instance, its definition, the `field_order` tuple and the FIELDS value tuples.
    assert_eq!(stats.objects_copied, 3 + FIELDS);
    assert_eq!(
        size_object(copied),
        live_words,
        "the flat size is exactly the words the copy wrote"
    );

    // The instance header is `MAKE_RECORD_HEADER(n) == n + 1`, and its definition word was
    // forwarded into the to-space.
    // SAFETY: `copied` is the live, copied record instance, so word 0 is its header.
    let inst = boxed_val(copied);
    assert_eq!(unsafe { *inst }, make_record_header(FIELDS + 1));
    assert_eq!(
        unsafe { record_field_count(copied) }.expect("field count"),
        FIELDS
    );
    // SAFETY: `copied` is a validated, live record instance.
    let def = unsafe { record_definition(copied) }.expect("definition");
    assert!(
        to.contains_term(def),
        "the instance's definition word must be relocated"
    );

    // The definition is a transparent arityval of `RECORD_DEF_SIZE(n) - 1 == n + 5`, whose
    // `module`/`name`/`is_exported`/`hash` words and sorted key words survived.
    // SAFETY: `def` is the live, copied definition (a `6 + n`-word block).
    let def_ptr = boxed_val(def);
    assert_eq!(
        unsafe { *def_ptr },
        make_arityval(FIELDS + 5),
        "the definition header is `make_arityval(RECORD_DEF_SIZE(n) - 1)`"
    );
    // SAFETY: `def` is a validated, live definition.
    unsafe {
        assert_eq!(
            record_definition_module(def).expect("module"),
            make_atom(module)
        );
        assert_eq!(record_definition_name(def).expect("name"), make_atom(name));
        assert_eq!(
            record_definition_is_exported(def).expect("is_exported"),
            make_atom(AM_TRUE)
        );
        assert_eq!(
            record_definition_hash(def).expect("hash"),
            make_small(0x1234),
            "the (immediate) hash word survives verbatim"
        );
    }
    // SAFETY: `def` is a validated, live definition.
    let keys = unsafe { record_definition_keys(def) }.expect("keys");
    let expected_keys = [
        make_atom(alpha),
        make_atom(beta),
        make_atom(gamma),
    ];
    assert_eq!(
        keys,
        expected_keys.as_slice(),
        "keys are stored in ascending atom-index order"
    );
    // SAFETY: `def` is a validated, live definition.
    let order = unsafe { record_definition_field_order(def) }.expect("field order");
    let ranks: Vec<i64> = order.iter().map(|e| signed_value(*e)).collect();
    assert_eq!(
        ranks,
        vec![2, 0, 1],
        "`field_order[declaration index]` is the sorted rank"
    );
    // SAFETY: `def` is a validated, live definition.
    let declared = unsafe { record_declaration_keys(def) }.expect("declaration keys");
    assert_eq!(
        declared,
        vec![make_atom(gamma), make_atom(alpha), make_atom(beta)],
        "the declaration order round-trips through `field_order`"
    );

    // The `field_order` tuple (definition word 1) is itself a relocated object.
    // SAFETY: `def_ptr` heads the live definition, so word 1 is the boxed order tuple.
    let order_tup = unsafe { *def_ptr.add(1) };
    assert!(
        to.contains_term(order_tup),
        "the `field_order` tuple must be relocated"
    );
    // SAFETY: `order_tup` is the live, copied `field_order` tuple.
    assert_eq!(unsafe { *boxed_val(order_tup) }, make_arityval(FIELDS));

    // Every field **value** word was forwarded, and the values are stored in sorted-key
    // order (alpha, beta, gamma) — i.e. the tuples `{1,10}`, `{2,20}`, `{0,0}`.
    // SAFETY: `copied` is a validated, live record instance.
    let stored = unsafe { record_values(copied) }.expect("values");
    assert_eq!(stored.len(), FIELDS);
    for (slot, expected) in stored.iter().zip([(1i64, 10i64), (2, 20), (0, 0)]) {
        assert!(
            to.contains_term(*slot),
            "every field value must be relocated into the to-space"
        );
        // SAFETY: `slot` is a live, copied 2-element value tuple.
        let (a, b) = unsafe { (*boxed_val(*slot).add(1), *boxed_val(*slot).add(2)) };
        assert_eq!(signed_value(a), expected.0);
        assert_eq!(signed_value(b), expected.1);
    }

    // The key→value lookup still resolves on the copy (gamma is declared first, so its
    // value is the `{0,0}` tuple — the last one in sorted-key order).
    // SAFETY: `copied` is a validated, live record instance.
    let got = unsafe { record_get(copied, make_atom(gamma), &atoms) }
        .expect("record_get")
        .expect("the key must resolve");
    assert!(to.contains_term(got));
    // SAFETY: `got` is the live, copied value tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(got).add(1) }), 0);
    assert_eq!(signed_value(unsafe { *boxed_val(got).add(2) }), 0);
}

// ---------------------------------------------------------------------------
// 3. `ErlSubBits` match context over an on-heap bitstring
// ---------------------------------------------------------------------------

/// The one shape where `beam_gc::copy::move_size` **overrides**
/// `beam_term::layout::object_words`: a match context's `ErlSubBits` copies as its own
/// `ERL_SUB_BITS_SIZE` (5) words and forwards its `orig` word, whereas `object_words`
/// reports the fresh heap-bits window a C `copy_struct` would materialise.
///
/// The 5-word size is proven **structurally, not by formula**: the copier forwards the root
/// first and then Cheney-scans the to-space, so the forwarded `orig` copy must land exactly
/// 5 words behind the `ErlSubBits` copy. A copy that laid down 8 words (the `object_words`
/// sizing of a refc-binary pair) would put it 8 words behind and cannot pass.
#[test]
fn sub_bits_match_context_over_an_on_heap_bitstring_copies_as_five_words() {
    // A 128-bit on-heap bitstring: `heap_bits_size_words(128) == 2 + 2 == 4` words.
    const N_BITS: usize = 128;
    const N_BYTES: usize = N_BITS / 8;
    let bytes: [u8; N_BYTES] = core::array::from_fn(|i| 0xA5u8.wrapping_mul(i as u8 + 1));
    let bits_words = heap_bits_size_words(N_BITS);
    assert_eq!(bits_words, 4);

    // The bit window the match context selects: bits 16..80 (64 bits).
    let start = 16usize;
    let end = 80usize;

    let mut from = space(bits_words + ERL_SUB_BITS_SIZE + 3);
    // SAFETY: `bits_words` exclusively-owned words are reserved at `hp`; `write_heap_bits`
    // writes all of them and the block stays live in `from`.
    let heap_bits = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(bits_words)
            .expect("heap-bits block");
        write_heap_bits(hp, &bytes, N_BITS).expect("bitstring")
    };
    // SAFETY: `heap_bits` is the live block just written; its data starts at word 2.
    let data_base = unsafe { boxed_val(heap_bits).add(ERL_HEAP_BITS_SIZE) };
    // A *real* match context: `flags == ERL_SUB_BITS_FLAG_MUTABLE` is exactly the state in
    // which the C allows `orig` to name an on-heap bitstring instead of a `BinRef`.
    let base_flags = Eterm::from_raw(data_base as usize | ERL_SUB_BITS_FLAG_MUTABLE);
    // SAFETY: `ERL_SUB_BITS_SIZE` (5) exclusively-owned words are reserved at `hp`;
    // `write_sub_bits` writes all of them, `base_flags` names storage covering bits
    // `start..end` of the live bitstring, and `orig` is a live boxed term.
    let ctx = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(ERL_SUB_BITS_SIZE)
            .expect("ErlSubBits block");
        write_sub_bits(
            hp,
            base_flags,
            Eterm::from_raw(start),
            Eterm::from_raw(end),
            heap_bits,
        )
    };
    assert_eq!(from.used_words(), bits_words + ERL_SUB_BITS_SIZE);
    // SAFETY: `ctx` is the live `ErlSubBits` just written.
    unsafe {
        assert_eq!(*boxed_val(ctx), HEADER_SUB_BITS);
        assert!(is_bitstring(ctx), "a sub-bitstring is still a bitstring");
    }

    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");

    // A to-space sized *exactly* to the 5-word `ErlSubBits` plus the 4-word on-heap
    // bitstring its `orig` forwards to.
    let live_words = ERL_SUB_BITS_SIZE + bits_words;
    let mut to = space(live_words);
    let (copied, stats) = collect_lone_root(&from, &mut to, ctx, live_words);
    assert_eq!(stats.objects_copied, 2, "the `ErlSubBits` + its bitstring");

    // SAFETY: `copied` is the live, copied `ErlSubBits`.
    let (copied_base_flags, copied_start, copied_end, copied_orig) =
        unsafe { sub_bits_parts(copied) };
    // The block itself is copied *verbatim*: `start`/`end` are the bit window and survive
    // bit-for-bit. `base_flags` is deliberately **not** rebased — this copier lays down the
    // 5 words and forwards only `orig`, whereas the legacy C additionally rewrites
    // `base_flags` onto the copied heap-bits block (`erl_sub_bits_update_moved`,
    // `erl_bits.h:521-534`); so only its flag bits are asserted here.
    assert_eq!(copied_start.raw(), start);
    assert_eq!(copied_end.raw(), end);
    assert_eq!(
        copied_base_flags.raw() & ERL_SUB_BITS_FLAG_MASK,
        ERL_SUB_BITS_FLAG_MUTABLE,
        "the match-context flag bits survive the verbatim block copy"
    );

    // `orig` (word 4) was forwarded to a fresh copy of the on-heap bitstring. The two
    // objects are laid down back to back by one Cheney scan, so the distance between them
    // *is* the `ErlSubBits` copy size — exactly `ERL_SUB_BITS_SIZE` (5).
    assert!(
        to.contains_term(copied_orig),
        "`orig` must be forwarded into the to-space"
    );
    // SAFETY: `copied_orig` is the live, copied boxed term `orig` now names.
    assert!(is_bitstring_header(unsafe { *boxed_val(copied_orig) }));
    // SAFETY: `copied` heads the live 5-word `ErlSubBits`.
    assert_eq!(
        boxed_val(copied_orig),
        unsafe { boxed_val(copied).add(ERL_SUB_BITS_SIZE) },
        "the `ErlSubBits` copy is exactly {ERL_SUB_BITS_SIZE} words, so the `orig` copy \
         lands immediately behind it"
    );

    // The copied on-heap bitstring is byte-identical: same payload, same bit size.
    // SAFETY: `copied_orig` is the live, copied bitstring.
    let (base, off, size) = unsafe { bitstring_parts(copied_orig) };
    assert_eq!(off, 0);
    assert_eq!(size, N_BITS);
    // SAFETY: `base` points at the copied block's `ceil(128/8) == 16` payload bytes.
    let got = unsafe { core::slice::from_raw_parts(base, N_BYTES) };
    assert_eq!(got, &bytes[..], "the copied payload must be byte-identical");

    // The *sizing* graph deliberately diverges here — the documented `ErlSubBits` override:
    // `size_object` reports the fresh `heap_bits_size_words(end - start)` window the C
    // `copy_struct` would materialise, not the 5-word block plus the whole underlying
    // bitstring the collector actually copied.
    assert_eq!(size_object(copied), heap_bits_size_words(end - start));
    assert_ne!(
        size_object(copied),
        to.used_words(),
        "the sizing walk and the copy walk are documented to differ for `ErlSubBits`"
    );
}

// ---------------------------------------------------------------------------
// 4. External port / external reference leaves
// ---------------------------------------------------------------------------

/// The **external port** (`EXTERNAL_PORT_SUBTAG`, 4 words) and **external reference**
/// (`EXTERNAL_REF_SUBTAG`, `3 + n/2 + 1` words) are leaf things: their payload words are
/// never child slots (`beam_term::layout` yields none for them — the C `node`/`next` slots
/// are off-heap pointers), so a collection must relocate each whole block intact and size it
/// exactly.
#[test]
fn external_port_and_reference_leaves_are_relocated_and_exactly_sized() {
    let mut atoms = AtomTable::new();
    // The `node` slot carries the sysname **atom** (the M1 substitution for the C off-heap
    // node pointer), i.e. an immediate — never a heap reference, which is what makes the
    // leaf's "no child slots" rule safe.
    let node = make_atom(atoms.intern("nonode@nohost"));
    const CREATION: u32 = 7;

    let port_words = external_port_size_words();
    let n_ids = 3usize;
    let ref_words = external_ref_size_words(n_ids);
    assert_eq!(port_words, 4, "EXTERNAL_PORT_HEAP_SIZE");
    assert_eq!(ref_words, 5, "3 + 3/2 + 1 words on ARCH_64");
    let ids = [0x1111_2222u32, 0x3333_4444, 0x5555_6666];
    const PORT_ID: u64 = 0x1234_5678_9ABC_DEF0;

    let mut from = space(port_words + ref_words + 3);
    // SAFETY: `port_words` (4) exclusively-owned words are reserved at `hp`;
    // `write_external_port` writes all of them and `node` is an immediate term.
    let port = unsafe {
        let hp = from.heap_mut().alloc_words(port_words).expect("port block");
        write_external_port(hp, node, PORT_ID, CREATION)
    };
    // SAFETY: `ref_words` (5) exclusively-owned words are reserved at `hp`;
    // `write_external_ref` writes all of them and `ids.len() <= MAX_REF_IDS`.
    let rref = unsafe {
        let hp = from.heap_mut().alloc_words(ref_words).expect("ref block");
        write_external_ref(hp, node, &ids, CREATION)
    };
    assert_eq!(from.used_words(), port_words + ref_words);

    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");

    // A to-space sized exactly to both leaves.
    let live_words = port_words + ref_words;
    let mut to = space(live_words);
    let mut roots = [port, rref];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        to.used_words(),
        live_words,
        "exactly the two leaf blocks are copied"
    );
    assert_eq!(stats.words_copied, live_words);
    assert_eq!(stats.objects_copied, 2);
    assert_eq!(stats.words_reclaimed, 3, "only the {{7,8}} garbage tuple");
    for root in roots {
        assert!(
            to.contains_term(root),
            "a leaf root must point into the to-space"
        );
        assert!(!from.contains_term(root));
    }

    // SAFETY: both roots are the live, copied leaves.
    unsafe {
        assert!(is_external_port_header(*boxed_val(roots[0])));
        assert!(is_external_ref_header(*boxed_val(roots[1])));
        assert_eq!(
            external_port_parts(roots[0]),
            (node, PORT_ID, CREATION),
            "every port word survives verbatim"
        );
        let (ref_node, ref_ids, ref_creation) = external_ref_parts(roots[1]);
        assert_eq!(ref_node, node);
        assert_eq!(ref_ids, &ids[..], "the id words survive verbatim");
        assert_eq!(ref_creation, CREATION);
    }
    // Each block's flat size is exactly the frozen `EXTERNAL_*_HEAP_SIZE` shape.
    assert_eq!(size_object(roots[0]), port_words);
    assert_eq!(size_object(roots[1]), ref_words);
}

// ---------------------------------------------------------------------------
// 5. HAMT head-array node
// ---------------------------------------------------------------------------

/// The key set for the head-array test: `i -> 3*i` for a key set that (a) has more than
/// [`MAP_SMALL_MAP_LIMIT`] members, so `write_map` builds a hash map at all, and (b) covers
/// all 16 level-0 HAMT nibbles (`erts_map_hash(key) & 0xF`), so the root's occupancy bitmap
/// is full — the `MAP_HEADER_TAG_HAMT_HEAD_ARRAY` trigger.
///
/// Keys are pushed in ascending order, so the returned pair list is already in the term
/// order `map_pairs` reports.
fn head_array_pairs() -> Vec<(Eterm, Eterm)> {
    let mut seen = [false; 16];
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::new();
    let mut i = 0i64;
    // The iteration bound turns a coverage failure into a clean assertion instead of a
    // hang; small integers comfortably cover all 16 nibbles.
    while (pairs.len() <= MAP_SMALL_MAP_LIMIT || seen.iter().any(|s| !s)) && i < 100_000 {
        let key = make_small(i);
        seen[(erts_map_hash(key) & 0xF) as usize] = true;
        pairs.push((key, make_small(i * 3)));
        i += 1;
    }
    assert!(
        seen.iter().all(|s| *s),
        "the 16 level-0 nibbles must all be covered"
    );
    assert!(
        pairs.len() > MAP_SMALL_MAP_LIMIT,
        "the map must be a hash map, not a flat map"
    );
    pairs
}

/// A **HAMT head-array node** — the `[hdr][size][16 slots]` root `write_map` emits once its
/// root occupancy bitmap is full — must be copied with its header, `size` word and *all 16*
/// slots forwarded, and sized exactly.
///
/// The footprint is measured **independently of every size heuristic** with the `MARKER`
/// pre-fill + scan-back technique of `map_copy.rs`. One pair's value is a boxed tuple, so
/// the leaf cons cell's forwarded `cdr` is exercised too.
#[test]
fn hamt_head_array_node_is_copied_intact_and_exactly_sized() {
    let atoms = AtomTable::new();
    const VALUE_WORDS: usize = 3; // the boxed value tuple `{7,8}`
    let cap = map_words_capacity(MAP_SMALL_MAP_LIMIT + 1);

    // A semi-space generously sized for the (not yet reserved) map block; only the words
    // actually reserved below count as used.
    let mut from = space(VALUE_WORDS + cap + 3);
    let boxed_value = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("boxed value");

    let mut pairs = head_array_pairs();
    let n = pairs.len();
    // Key 0's value becomes the boxed tuple, so one leaf cell has a forwarded reference.
    pairs[0].1 = boxed_value;

    // Pass 1: measure the map's contiguous footprint (which is independent of the *value*
    // words — `write_map` orders and dedups on the keys alone) by pre-filling a scratch
    // reservation with `MARKER` and scanning back to the last word `write_map` overwrote.
    let footprint = {
        let mut scratch = space(cap);
        // SAFETY: `cap` exclusively-owned, writable words are reserved; the marker fill is a
        // plain write and `write_map` then overwrites a contiguous prefix with well-formed
        // terms (the boxed value word is stored verbatim, never dereferenced).
        unsafe {
            let hp = scratch
                .heap_mut()
                .alloc_words(cap)
                .expect("reserve the scratch block");
            for k in 0..cap {
                *hp.add(k) = Eterm::from_raw(MARKER);
            }
            write_map(hp, &pairs, &atoms).expect("build the scratch map");
            let mut end = cap;
            while end > 0 && (*hp.add(end - 1)).raw() == MARKER {
                end -= 1;
            }
            end
        }
    };
    assert!(
        footprint > 0 && footprint < cap,
        "the map under-fills its sound upper bound"
    );

    // Pass 2: build the real map with an *exact* reservation (`write_map` is deterministic,
    // so it writes precisely `footprint` words again).
    // SAFETY: `footprint` was just measured as exactly the words `write_map` writes, so the
    // reservation is exact; every written word stays live in `from`.
    let map = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(footprint)
            .expect("reserve the exact map block");
        write_map(hp, &pairs, &atoms).expect("build the map")
    };
    let live_words = VALUE_WORDS + footprint;
    assert_eq!(from.used_words(), live_words);

    // The root node header is *explicitly* the head-ARRAY subtag, with a full 16-slot
    // occupancy bitmap — the shape under test, not a head-bitmap or an inner node.
    // SAFETY: `map` is the live map root just written, so word 0 is the node header.
    let hdr = unsafe { *boxed_val(map) };
    assert_eq!(
        map_header_type(hdr) as usize,
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY
    );
    assert_eq!(
        hdr.raw() & HAMT_SUBTAG_HEAD_ARRAY,
        HAMT_SUBTAG_HEAD_ARRAY,
        "the header word carries `HAMT_SUBTAG_HEAD_ARRAY`"
    );
    assert_eq!(map_header_val(hdr), 0xffff, "a full 16-slot bitmap");
    assert!(is_map_header(hdr) && !is_flatmap_header(hdr));
    // `[hdr][size][16 slots]` — 18 words, the per-object size the copier must use.
    assert_eq!(unsafe { object_words(hdr, boxed_val(map)) }, 18);

    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(99), make_small(100)])
        .expect("garbage tuple");

    // A to-space sized exactly to the map block plus the boxed value.
    let mut to = space(live_words);
    let (copied, stats) = collect_lone_root(&from, &mut to, map, live_words);
    // The head node, one `[Key, Value]` cons cell per key, the boxed value and any inner /
    // collision nodes — never fewer than `n + 2`.
    assert!(
        stats.objects_copied >= n + 2,
        "every one of the {n} leaf cells plus the head node and boxed value must be copied"
    );

    // The head node's header, `size` word and all 16 slots were copied verbatim/forwarded.
    // SAFETY: `copied` is the live, copied head-array node.
    let head = boxed_val(copied);
    assert_eq!(
        unsafe { *head },
        hdr,
        "the node header word is copied byte-for-byte"
    );
    assert_eq!(
        unsafe { *head.add(1) }.raw(),
        n,
        "the head node's `size` word is the map's key count"
    );
    for slot_index in 0..16 {
        // SAFETY: the head-array node owns 18 words, so words 2..18 are its 16 slots.
        let slot = unsafe { *head.add(2 + slot_index) };
        assert!(
            to.contains_term(slot),
            "head-array slot {slot_index} must be forwarded into the to-space"
        );
    }

    // Every pair round-trips through the copied trie, in key order; the boxed value was
    // relocated (the leaf cell's `cdr` was forwarded) and the rest survive verbatim.
    assert_eq!(map_size(copied, &atoms).expect("map_size"), n);
    for &(key, value) in &pairs {
        let got = map_get(copied, key, &atoms)
            .expect("map_get")
            .expect("every key must resolve on the copy");
        if value == boxed_value {
            assert!(to.contains_term(got), "the boxed value must be relocated");
            // SAFETY: `got` is the live, copied 2-element value tuple.
            let (a, b) = unsafe { (*boxed_val(got).add(1), *boxed_val(got).add(2)) };
            assert_eq!(signed_value(a), 7);
            assert_eq!(signed_value(b), 8);
        } else {
            assert_eq!(got, value, "an immediate value survives verbatim");
        }
    }
    let keys: Vec<Eterm> = map_pairs(copied, &atoms)
        .expect("map_pairs")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let expected: Vec<Eterm> = pairs.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, expected, "the key set and its order must round-trip");

    // The independent flat size agrees with the copy's exact accounting.
    assert_eq!(size_object(copied), live_words);

    // Sanity: `make_map_header` rebuilds the same header word.
    assert_eq!(
        hdr,
        make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff)
    );
}
