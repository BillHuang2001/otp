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

//! Copy-correctness + sizing evidence for the **fun** term shape through a collection.
//!
//! A fun is a boxed thing whose block is `[header][desc_words(kind)][env[0..num_free]]`
//! (`beam_term::fun`): the C `entry` pointer is expanded into **inlined descriptor
//! words** — 2 for a `Local` fun (`uniq`·`index`), 3 for an `External` fun
//! (`module`·`function`·`arity`), 10 for an `Imported` fun (the whole `NEW_FUN_EXT`
//! signature). `beam_term::header_arity`/`thing_arityval` deliberately keep the C
//! `ERL_FUN_SIZE - 1 == 1` value (the old single-`entry` assumption), so a copier that
//! sized or scanned a fun through `thing_arityval + 1` would (a) **under-size** the copy
//! by `desc_words(kind) - 1` words — truncating the environment, and for an `External`
//! fun dropping the descriptor `arity` word — and (b) advance the Cheney scan onto a raw
//! descriptor word (`index` for `Local`, the `arity` word for `External`, `uniq` bytes
//! for `Imported`) and follow it as a bogus term reference, corrupting the heap.
//!
//! These tests build all three kinds in a real [`HeapSpace`] (with compound free
//! variables that must themselves be relocated), collect them as roots, and then assert
//! **from the updated roots** that the kind / metadata / environment all round-trip, that
//! every boxed free variable moved into the to-space, and that the to-space holds
//! *exactly* the layout-formula footprint — proving the copier neither under- nor
//! over-sizes a fun.
//!
//! They exercise the public API only; every `unsafe` block carries a `// SAFETY:`
//! rationale and only touches words this test allocated itself.

use beam_alloc::default_allocator;
use beam_gc::prelude::{collect, HeapSpace};
use beam_term::prelude::{
    boxed_val, fun_arity, fun_block_words, fun_creator_pid, fun_desc_words, fun_env, fun_function,
    fun_index, fun_kind_of_header, fun_module, fun_num_free, fun_true_arity, fun_uniq,
    fun_uniq_bytes, fun_words, list_val, make_arityval, make_boxed, make_small, write_fun_external,
    write_fun_imported, write_fun_local, AtomTable, Eterm, FunKind, FunPid, NIL,
};

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// The header word of a live boxed term (the fun block's word 0).
fn header(t: Eterm) -> Eterm {
    // SAFETY: `t` is a live boxed term the owning heap keeps unmoved for the test.
    unsafe { *boxed_val(t) }
}

/// Reserves exactly [`fun_words`] words from `from` and writes a `Local` fun there.
///
/// Reserving the layout-formula size is deliberate: the from-space then holds exactly
/// the words the block claims, so the copy's `used_words` proves the size.
fn write_local(from: &mut HeapSpace, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Local, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved at `hp`; every one
    // is written by `write_fun_local`, and each `env` entry is a live term (or an
    // immediate), meeting the writer's contract.
    unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(words)
            .expect("reserve local fun");
        write_fun_local(hp, arity, uniq, index, env).expect("write local fun")
    }
}

/// Reserves exactly [`fun_words`] words from `from` and writes an `External` fun there.
fn write_external(
    from: &mut HeapSpace,
    arity: usize,
    module: beam_term::prelude::Atom,
    function: beam_term::prelude::Atom,
) -> Eterm {
    let words = fun_words(FunKind::External, 0);
    // SAFETY: `words` (4) exclusively-owned, writable words are reserved at `hp`; every
    // one is written by `write_fun_external`.
    unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(words)
            .expect("reserve external fun");
        write_fun_external(hp, arity, module, function).expect("write external fun")
    }
}

/// Reserves exactly [`fun_words`] words from `from` and writes an `Imported` fun there.
#[allow(clippy::too_many_arguments)]
fn write_imported(
    from: &mut HeapSpace,
    arity: usize,
    module: beam_term::prelude::Atom,
    uniq: &[u8; 16],
    index: u32,
    creator: FunPid,
    env: &[Eterm],
) -> Eterm {
    let words = fun_words(FunKind::Imported, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved at `hp`; every one
    // is written by `write_fun_imported`, and each `env` entry is a live term (or an
    // immediate), meeting the writer's contract.
    unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(words)
            .expect("reserve imported fun");
        write_fun_imported(hp, arity, module, uniq, index, 0, 0, creator, env)
            .expect("write imported fun")
    }
}

/// The per-kind block size is `1 + desc_words(kind) + num_free`, and the two public
/// sizing APIs (`fun_words`, `fun_block_words`) agree with it — pinning the formula a
/// future change must not break.
#[test]
fn fun_block_size_formula_is_pinned_per_kind() {
    // The inlined descriptor widths (`beam_term::fun`).
    assert_eq!(fun_desc_words(FunKind::Local), 2);
    assert_eq!(fun_desc_words(FunKind::External), 3);
    assert_eq!(fun_desc_words(FunKind::Imported), 10);

    // `fun_words` == `1 + desc + num_free` across the kind/size grid.
    for num_free in [0usize, 1, 3, 200, 255] {
        for kind in [FunKind::Local, FunKind::External, FunKind::Imported] {
            let expect = 1 + fun_desc_words(kind) + num_free;
            assert_eq!(fun_words(kind, num_free), expect, "{kind:?} nf={num_free}");
        }
    }

    // A real written block reports that same size through `fun_block_words` — the value
    // the copier's `move_size` must reproduce.
    let mut from = space(64);
    let free = [make_small(1), make_small(2), make_small(3)];

    assert_eq!(fun_words(FunKind::Local, 3), 6);
    let local = write_local(&mut from, 4, 0xABCD, 7, &free);
    assert_eq!(fun_block_words(local).unwrap(), 6);

    let mut atoms = AtomTable::new();
    atoms.seed(["mod", "call"]);
    let module = atoms.intern("mod");
    let function = atoms.intern("call");

    assert_eq!(fun_words(FunKind::External, 0), 4);
    let external = write_external(&mut from, 2, module, function);
    assert_eq!(fun_block_words(external).unwrap(), 4);

    assert_eq!(fun_words(FunKind::Imported, 1), 12); // 1 + 10 + 1
    let imported = write_imported(
        &mut from,
        1,
        module,
        &[0u8; 16],
        9,
        FunPid {
            node: module,
            number: 1,
            serial: 2,
            creation: 3,
        },
        &[make_small(42)],
    );
    assert_eq!(fun_block_words(imported).unwrap(), 12);
}

/// All three fun kinds — with compound free variables — survive a collection, and the
/// to-space holds *exactly* the layout-formula live footprint.
#[test]
fn funs_of_all_three_kinds_survive_a_collection() {
    let mut atoms = AtomTable::new();
    atoms.seed(["mod", "call", "node"]);
    let module = atoms.intern("mod");
    let function = atoms.intern("call");
    let node = atoms.intern("node");

    let mut from = space(64);

    // Compound free variables, allocated in the *same* from-space, that must be
    // relocated by the Cheney scan of the funs' environments.
    let t = from
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("tuple"); // 3 words
    let c = from.heap_mut().cons(make_small(3), NIL).expect("cons"); // 2 words
    let t2 = from
        .heap_mut()
        .tuple(&[make_small(6), make_small(7), make_small(8)])
        .expect("tuple"); // 4 words
    let c2 = from.heap_mut().cons(make_small(9), NIL).expect("cons"); // 2 words

    // Local fun: env = [t, c, 5] (one compound tuple, one cons, one immediate).
    let local = write_local(&mut from, 3, 0xDEAD_BEEF, 11, &[t, c, make_small(5)]);
    // External fun: no environment.
    let external = write_external(&mut from, 2, module, function);
    // Imported fun: env = [t2, c2] (a compound tuple and cons).
    let uniq = [
        0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];
    let creator = FunPid {
        node,
        number: 4,
        serial: 5,
        creation: 6,
    };
    let imported = write_imported(&mut from, 1, module, &uniq, 21, creator, &[t2, c2]);

    // Deliberate garbage: an unreferenced 3-word `{99,98}` tuple.
    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(99), make_small(98)])
        .expect("garbage tuple");

    // The exact live footprint, by the layout formula: the three fun blocks plus the
    // four compound free-variable objects the environment reaches.
    let local_w = fun_words(FunKind::Local, 3); // 6
    let external_w = fun_words(FunKind::External, 0); // 4
    let imported_w = fun_words(FunKind::Imported, 2); // 13
    let expected_live = local_w + external_w + imported_w + 3 + 2 + 4 + 2;
    assert_eq!(
        from.used_words(),
        expected_live + 3,
        "the from-space holds the whole live graph plus the 3-word garbage"
    );

    // A to-space sized *exactly* to the live footprint: the copy must fit precisely.
    let mut to = space(expected_live);
    let mut roots = [local, external, imported];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // (a) Every root moved into the to-space, out of the from-space.
    for &r in &roots {
        assert!(to.contains_term(r), "root must point into the to-space");
        assert!(
            !from.contains_term(r),
            "root must no longer point into the from-space"
        );
    }

    // (b) Each kind and its metadata are preserved.
    let (local, external, imported) = (roots[0], roots[1], roots[2]);

    assert_eq!(fun_kind_of_header(header(local)), FunKind::Local);
    assert_eq!(fun_arity(header(local)), 3);
    assert_eq!(fun_num_free(header(local)), 3);
    assert_eq!(fun_block_words(local).unwrap(), local_w);
    // SAFETY: `local` is the live, copied local fun.
    unsafe {
        assert_eq!(fun_uniq(local).unwrap(), 0xDEAD_BEEF);
        assert_eq!(fun_index(local).unwrap(), 11);
    }

    assert_eq!(fun_kind_of_header(header(external)), FunKind::External);
    assert_eq!(fun_arity(header(external)), 2);
    assert_eq!(fun_num_free(header(external)), 0);
    assert_eq!(fun_block_words(external).unwrap(), external_w);
    // SAFETY: `external` is the live, copied external fun.
    unsafe {
        assert_eq!(fun_module(external).unwrap(), module);
        assert_eq!(fun_function(external).unwrap(), function);
        assert_eq!(fun_true_arity(external).unwrap(), 2);
    }

    assert_eq!(fun_kind_of_header(header(imported)), FunKind::Imported);
    assert_eq!(fun_arity(header(imported)), 1);
    assert_eq!(fun_num_free(header(imported)), 2);
    assert_eq!(fun_block_words(imported).unwrap(), imported_w);
    // SAFETY: `imported` is the live, copied imported fun.
    unsafe {
        assert_eq!(fun_module(imported).unwrap(), module);
        assert_eq!(fun_index(imported).unwrap(), 21);
        assert_eq!(fun_uniq_bytes(imported).unwrap(), uniq);
        assert_eq!(fun_creator_pid(imported).unwrap(), creator);
    }

    // (c) Every compound free variable was relocated into the to-space, and the
    // immediate one is unchanged.
    // SAFETY: `local` is the live, copied 6-word local fun.
    let local_env = unsafe { fun_env(local) }.unwrap();
    assert_eq!(local_env.len(), 3);
    assert!(
        to.contains_term(local_env[0]),
        "the boxed tuple free variable must be relocated into the to-space"
    );
    assert!(
        !from.contains_term(local_env[0]),
        "the boxed tuple free variable must no longer point into the from-space"
    );
    assert!(
        to.contains_term(local_env[1]),
        "the cons free variable must be relocated into the to-space"
    );
    // The relocated tuple is still `{1,2}`.
    // SAFETY: `local_env[0]` is the live, copied tuple (a term word inside the to-space).
    let tv = boxed_val(local_env[0]);
    assert_eq!(unsafe { *tv }, make_arityval(2));
    assert_eq!(unsafe { *tv.add(1) }, make_small(1));
    assert_eq!(unsafe { *tv.add(2) }, make_small(2));
    // The relocated cons is still `[3 | []]`.
    // SAFETY: `local_env[1]` is the live, copied cons (a term word inside the to-space).
    let cv = list_val(local_env[1]);
    assert_eq!(unsafe { *cv }, make_small(3));
    assert_eq!(unsafe { *cv.add(1) }, NIL);
    assert_eq!(
        local_env[2],
        make_small(5),
        "the immediate env word is unchanged"
    );

    // SAFETY: `imported` is the live, copied 13-word imported fun.
    let imported_env = unsafe { fun_env(imported) }.unwrap();
    assert_eq!(imported_env.len(), 2);
    assert!(
        to.contains_term(imported_env[0]) && to.contains_term(imported_env[1]),
        "the imported fun's compound free variables must be relocated"
    );
    // SAFETY: `imported_env[0]` is the live, copied `{6,7,8}` tuple.
    let tv2 = boxed_val(imported_env[0]);
    assert_eq!(unsafe { *tv2 }, make_arityval(3));
    assert_eq!(unsafe { *tv2.add(1) }, make_small(6));
    assert_eq!(unsafe { *tv2.add(2) }, make_small(7));
    assert_eq!(unsafe { *tv2.add(3) }, make_small(8));

    // (d) No under/over-sizing: the to-space holds exactly the layout-formula
    // footprint (fun blocks by the formula + the relocated compound objects), and the
    // reclaimed words are exactly the deliberate garbage.
    assert_eq!(
        to.used_words(),
        expected_live,
        "the copy is exactly the layout-formula live footprint"
    );
    assert_eq!(stats.words_copied, expected_live);
    assert_eq!(stats.objects_copied, 7, "3 funs + 2 tuples + 2 cons cells");
    assert_eq!(stats.words_reclaimed, 3, "only the {{99,98}} garbage tuple");
}

/// A fun's **descriptor words are opaque** to the Cheney scan: only the `env` words
/// may be followed as term references.
///
/// The scan must advance a fun header by `fun_env_offset_header` (`1 + desc_words`),
/// *not* `thing_arityval + 1` (`2`). To prove the difference is observable — and not
/// merely a lucky alignment — this test plants a raw descriptor word that *does* alias a
/// live from-space pointer: an `Imported` fun whose `uniq[0..8]` is a BOXED-tagged
/// pointer to a live tuple. A scan that treated that word as a reference would (a)
/// garbage-copy the aliased tuple and (b) overwrite the copied fun's `uniq` word with a
/// forwarding marker — corrupting the term. The correct scan skips the whole descriptor
/// opaquely, so the fun's `uniq` round-trips and the aliased tuple stays garbage.
#[test]
fn fun_descriptor_words_are_not_followed_as_references() {
    let mut atoms = AtomTable::new();
    atoms.seed(["mod", "node"]);
    let module = atoms.intern("mod");
    let node = atoms.intern("node");

    let mut from = space(32);

    // A live from-space object the crafted descriptor word will alias. It is reachable
    // only through that raw word, so a correct scan leaves it behind as garbage.
    let aliased = from
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("aliased tuple"); // 3 words
    let aliased_pointer = boxed_val(aliased);

    // `uniq[0..8]` = the little-endian BOXED-tagged pointer to the aliased tuple, i.e.
    // raw descriptor word 2 of the imported fun. `uniq[8..16]` stays zero.
    let mut uniq = [0u8; 16];
    let aliasing = make_boxed(aliased_pointer).raw() as u64;
    uniq[0..8].copy_from_slice(&aliasing.to_le_bytes());

    let imported = write_imported(
        &mut from,
        1,
        module,
        &uniq,
        0,
        FunPid {
            node,
            number: 0,
            serial: 0,
            creation: 0,
        },
        &[],
    );
    let fun_w = fun_words(FunKind::Imported, 0); // 11
    assert_eq!(from.used_words(), 3 + fun_w);

    let mut to = space(fun_w);
    let mut roots = [imported];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    let copied = roots[0];
    assert!(to.contains_term(copied));
    assert!(!from.contains_term(copied));

    // The crafted descriptor survived verbatim: it was never followed.
    // SAFETY: `copied` is the live, copied imported fun.
    assert_eq!(unsafe { fun_uniq_bytes(copied) }.unwrap(), uniq);
    // The to-space holds exactly the fun block; the aliased tuple was NOT copied.
    assert_eq!(
        to.used_words(),
        fun_w,
        "only the fun block is copied; the aliased tuple is garbage"
    );
    assert_eq!(stats.words_copied, fun_w);
    assert_eq!(stats.objects_copied, 1);
    assert_eq!(stats.words_reclaimed, 3, "the aliased tuple is reclaimed");
}

/// With an all-immediate environment the *only* copied words are the three fun blocks,
/// so the to-space is exactly their sum — the tightest possible sizing proof.
#[test]
fn fun_copy_total_is_exactly_the_sum_of_the_three_fun_blocks() {
    let mut atoms = AtomTable::new();
    atoms.seed(["m", "f"]);
    let module = atoms.intern("m");
    let function = atoms.intern("f");

    let mut from = space(32);
    let local = write_local(&mut from, 2, 5, 6, &[make_small(1), make_small(2)]);
    let external = write_external(&mut from, 0, module, function);
    let imported = write_imported(
        &mut from,
        3,
        module,
        &[0xABu8; 16],
        7,
        FunPid {
            node: module,
            number: 1,
            serial: 2,
            creation: 3,
        },
        &[make_small(9)],
    );

    let sum = fun_words(FunKind::Local, 2)
        + fun_words(FunKind::External, 0)
        + fun_words(FunKind::Imported, 1);
    assert_eq!(
        from.used_words(),
        sum,
        "the from-space is exactly the three funs"
    );

    let mut to = space(sum);
    let mut roots = [local, external, imported];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(to.used_words(), sum, "exactly the three fun block sizes");
    assert_eq!(stats.words_copied, sum);
    assert_eq!(stats.objects_copied, 3);
    assert_eq!(stats.words_reclaimed, 0, "the whole from-space was live");
    // The roots still describe the right kinds.
    for (r, kind) in roots
        .iter()
        .zip([FunKind::Local, FunKind::External, FunKind::Imported])
    {
        assert_eq!(fun_kind_of_header(header(*r)), kind);
    }
}

/// The **descriptor `arity` word** of an `External` fun survives a copy: all 4 words of
/// the block (`[header][module][function][arity]`) are moved, so a true arity the
/// header's 8-bit field cannot hold is preserved by the descriptor word.
///
/// `write_fun_external` masks the header's 8-bit arity field and stores the *full* arity
/// in descriptor word 3 (reachable only through `fun_true_arity`). A copier that sized
/// the block as the old 3 words would truncate that word, and the copy would report only
/// the header's masked arity. Writing `arity = 300` (needing 9 bits) makes the two
/// observably different: `fun_arity(header(copied)) == 300 & 0xFF == 44` while
/// `fun_true_arity(copied) == 300`.
#[test]
fn external_fun_descriptor_arity_word_survives_a_copy() {
    let mut atoms = AtomTable::new();
    atoms.seed(["mod", "call"]);
    let module = atoms.intern("mod");
    let function = atoms.intern("call");

    let mut from = space(16);
    // A true arity past the header's 8-bit field (`300 & 0xFF == 44`).
    let external = write_external(&mut from, 300, module, function);
    assert_eq!(fun_words(FunKind::External, 0), 4);
    assert_eq!(
        from.used_words(),
        4,
        "the from-space is exactly the 4-word fun block"
    );

    // A to-space sized *exactly* to the 4-word block: the copy must fit precisely.
    let mut to = space(4);
    let mut roots = [external];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // The root moved into the to-space, out of the from-space.
    let copied = roots[0];
    assert!(to.contains_term(copied), "root must point into the to-space");
    assert!(
        !from.contains_term(copied),
        "root must no longer point into the from-space"
    );

    // All 4 words (header, `module`, `function`, descriptor `arity`) were copied.
    assert_eq!(
        to.used_words(),
        fun_words(FunKind::External, 0),
        "exactly the fun block is copied"
    );
    assert_eq!(stats.words_copied, 4);
    assert_eq!(stats.objects_copied, 1);
    assert_eq!(stats.words_reclaimed, 0, "the whole from-space was live");

    // The header's 8-bit field is honestly truncated (`300 & 0xFF == 44`)...
    assert_eq!(fun_arity(header(copied)), 300 & 0xFF);
    assert_ne!(
        fun_arity(header(copied)),
        300,
        "the header cannot hold the true arity"
    );
    // ...but the descriptor `arity` word survived the copy verbatim.
    // SAFETY: `copied` is the live, copied external fun.
    assert_eq!(unsafe { fun_true_arity(copied) }.unwrap(), 300);
}
