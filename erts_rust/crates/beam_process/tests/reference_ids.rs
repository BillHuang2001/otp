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

//! **PIN — HAZARD 2: the two independent reference-id spaces collide on `=:=`.**
//!
//! # The hazard
//!
//! This runtime mints references from **two independent allocators**, each a
//! [`RefAllocator::new()`] that is monotone from id `0`:
//!
//! * this crate's [`ProcessTable`] owns one — every `erlang:monitor/2,3` and `erlang:alias/0,1`
//!   draws from it ([`ProcessTable::create_alias`], `ProcessTable::monitor`);
//! * `beam_rt`'s `Runtime::refs` owns the other — `erlang:make_ref/0`, the NIF `make_ref`, the
//!   `erlang:send_after/3`/`start_timer/3` timer references and `seq_trace` draw from it.
//!
//! Both render a reference with the **same** frozen 4-word block. `MonitorRef::to_eterm` and
//! `beam_term::boxed::write_internal_ref` (and `beam_bifs`'s `make_ref_thing`) all write
//! `[make_header(3, REF_SUBTAG), node = 0, serial = 0, make_small(id)]`, and the second allocator
//! goes through the very same `MonitorRef::to_eterm` (`beam_rt::native`'s `mint_timer_ref`). Because
//! the two spaces both start at `0`, the first `make_ref/0` and the first monitor/alias reference
//! carry the **same id**, so they are literally the **same term**.
//!
//! Term comparison (`beam_term::compare::cmp_refs`) compares the header arity and then the payload
//! words — it never consults the header **subtag** — so the payload's single distinguishing word is
//! the id. Equal ids therefore compare `=:=`. The oracle has **one** id space, so it can never
//! produce such a pair: this is a divergence, not desirable behaviour.
//!
//! (The collision cannot be oracle-cross-checked: the oracle has a single id space, so it is not
//! possible to make it mint two references with the same id. That is exactly what makes this a
//! divergence from the oracle.)
//!
//! # What is already correct (and stays correct)
//!
//! `beam_rt` classifies a `send` destination by **provenance** (the reference term **word**), not by
//! its decoded id, so the *send* path never delivers a bare `make_ref/0` to a live alias it happens
//! to collide with
//! (`beam_rt/src/context/tests.rs::resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias`).
//! The residual is purely reference **identity** (`=:=`), which is what this file pins.
//!
//! # Why this cannot be closed from `beam_process`
//!
//! Closing it needs a change no crate here can make alone. Every avenue is blocked:
//!
//! 1. **Vary a payload word other than the id** (`node`/`serial`) — the only word the two spaces
//!    could disagree on without changing the id. `beam_codec`'s internal-reference codec is
//!    **id-only**: it encodes through `term::internal_ref_id` and rebuilds through
//!    `make_internal_ref` → `beam_term::boxed::write_internal_ref`, which **zeroes** `node` and
//!    `serial`. A non-zero `node`/`serial` would therefore break
//!    `binary_to_term(term_to_binary(M)) =:= M` — a case `beam_rt/tests/reference_ids.rs` asserts
//!    and the oracle cross-checks. Fixing that needs `beam_codec` (a sibling) to carry the whole
//!    payload.
//! 2. **Vary the header subtag** — `cmp_refs` compares arity first and the payload only, never the
//!    subtag, so a distinct subtag would not distinguish two refs of equal arity.
//! 3. **Vary the header arity** — a distinct arity would break `internal_ref_id`'s arity check and
//!    [`MonitorRef::from_eterm`], and would still fail to round-trip through the id-only codec.
//! 4. **Unify the two allocators onto one id space** — the real fix, but the second allocator is
//!    `beam_rt`'s (`Runtime::refs`): it must stop minting its own references (or draw from this
//!    table's allocator), a cross-crate change `beam_process` cannot make.
//! 5. **Change `RefAllocator::new()`'s starting value or band it per instance** — `beam_rt`'s unit
//!    test `resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias` relies on
//!    the space being **monotone from 0** (its `force_next_reference_id` advances a fresh
//!    `RefAllocator::new()` `id` times so the next draw lands on `id`), so changing the start value
//!    or banding per instance breaks that test — and `beam_rt` is a sibling edited concurrently.
//!
//! # Owner of the fix
//!
//! A cross-crate change, not one this node may make:
//!
//! * extend `beam_codec`'s internal-reference codec to preserve the whole `[node, serial, id]`
//!   payload (then a producer may vary `node`/`serial` per id space), **or**
//! * unify the two allocators via `beam_rt` (draw `make_ref/0` from the [`ProcessTable`]'s
//!   allocator).
//!
//! Until then this file pins the divergence and its exact cause. `beam_rt` carries the matching
//! provenance-layer pin in its own unit tests, and the crate `CONTEXT.md` records the facts.

use beam_alloc::Heap;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    boxed_val, make_boxed, make_header, make_small, signed_value, write_internal_ref, AtomTable,
    Eterm, REF_SUBTAG,
};

/// The four words of a reference term, read straight out of its on-heap block.
///
/// # Safety
///
/// Every term passed here is a live, four-word reference block built by one of the producers under
/// test (`MonitorRef::to_eterm`, `beam_term::boxed::write_internal_ref`, or the fabricated block
/// below).
fn ref_words(term: Eterm) -> [Eterm; 4] {
    // SAFETY: every argument is a live four-word ref block, so words 0..4 are in bounds.
    unsafe {
        let p = boxed_val(term);
        [*p, *p.add(1), *p.add(2), *p.add(3)]
    }
}

/// Pins the **root cause**: this crate's [`RefAllocator`] is monotone from `0`, so **two**
/// independent instances hand out the very same ids. Every consumer either owns one (the
/// [`ProcessTable`]) or — like `beam_rt`'s `Runtime::refs` — constructs its own, so the id spaces
/// coincide from the first draw.
#[test]
fn two_independent_reference_allocators_start_at_the_same_zero() {
    let mut a = RefAllocator::new();
    let mut b = RefAllocator::new();
    let a0 = a.next();
    let b0 = b.next();
    assert_eq!(
        a0.id(),
        0,
        "the first reference of a fresh allocator is id 0"
    );
    assert_eq!(b0.id(), 0, "and so is the first reference of a second one");
    assert_eq!(a0, b0, "the two id spaces coincide on their first draw");
    // …and on every subsequent draw, because both are the same monotone sequence.
    for step in 1..8 {
        assert_eq!(
            a.next().id(),
            step,
            "the allocator advances one id per draw"
        );
        assert_eq!(
            b.next().id(),
            step,
            "the second allocator tracks it exactly"
        );
    }
}

/// The **divergence**, forced deterministically through real crate objects: a reference the
/// [`ProcessTable`] minted for an `erlang:alias/1` alias and a reference an **independent**
/// allocator (the shape of `beam_rt`'s `Runtime::refs`) mints for a `make_ref/0` carry the same id,
/// so they are byte-for-byte the same block and compare `=:=`.
#[test]
fn the_process_table_and_an_independent_allocator_mint_the_same_reference_for_the_same_id() {
    // The process table's OWN allocator: `erlang:alias/0,1` and `erlang:monitor/2,3` draw here.
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let mut table = ProcessTable::new(atoms);
    let owner = table.spawn_default().expect("an alias owner");
    let alias = table
        .create_alias(owner, UnaliasMode::ExplicitUnalias)
        .expect("the owner is live");
    assert_eq!(alias.id(), 0, "the table's first reference is id 0");

    // An INDEPENDENT allocator — the shape of `beam_rt`'s `Runtime::refs` (fed by `make_ref/0`).
    let bare = RefAllocator::new().next();
    assert_eq!(
        bare.id(),
        alias.id(),
        "the two id spaces collide on the very first reference"
    );

    // Both render through the very same `MonitorRef::to_eterm`.
    let mut heap = Heap::new(16).expect("test heap");
    let alias_term = alias.to_eterm(&mut heap).expect("the alias ref fits");
    let bare_term = bare.to_eterm(&mut heap).expect("the bare ref fits");

    // The two terms are byte-identical: the frozen block leaves only the id to tell two references
    // apart, and the two spaces share it.
    assert_eq!(
        ref_words(alias_term),
        ref_words(bare_term),
        "the two references are the identical four-word block"
    );
    // …and decode back to the same `MonitorRef`.
    assert_eq!(
        MonitorRef::from_eterm(alias_term).expect("an alias produces a reference term"),
        MonitorRef::from_eterm(bare_term).expect("a bare ref produces a reference term"),
        "and to the same reference id"
    );

    // **The divergence**: `=:=` (and the arithmetic `==`) cannot tell them apart.
    let atoms = AtomTable::new();
    assert!(
        compare::eq(alias_term, bare_term, &atoms),
        "HAZARD 2: a bare make_ref/0 and a monitor/alias reference with the same id compare =:= \
         here — a divergence from the oracle's single id space"
    );
    assert!(
        compare::cmp_arith(alias_term, bare_term, &atoms).is_eq(),
        "the arithmetic == ordering agrees with =:= for two references"
    );
}

/// The runtime mints its **bare** references through `beam_term::boxed::write_internal_ref` (the
/// shape of `beam_bifs::make_ref_thing`); this pins that such a block and a
/// [`MonitorRef::to_eterm`] block with the same id are the same term too — the two producers write
/// the identical frozen block, so the collision is not an artefact of one writer.
#[test]
fn a_runtime_minted_bare_reference_and_a_monitor_reference_with_equal_ids_are_exactly_equal() {
    let id = 7u64;
    let mut heap = Heap::new(16).expect("test heap");

    // The runtime side: the canonical internal-reference writer, exactly what `make_ref/0` and the
    // NIF `make_ref` write.
    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: `hp` owns exactly four words, which `write_internal_ref` fully initialises; the id
    // fits a 60-bit small (it is 7).
    let bare = unsafe { write_internal_ref(hp, id) }.expect("the id fits a small");

    // The monitor side: a `MonitorRef` with the same id, from its own allocator.
    let mut monitor_side = RefAllocator::new();
    let mut reference = monitor_side.next();
    while reference.id() < id {
        reference = monitor_side.next();
    }
    assert_eq!(reference.id(), id, "the allocator reached the same id");
    let monitor_term = reference.to_eterm(&mut heap).expect("the monitor ref fits");

    assert_eq!(
        ref_words(bare),
        ref_words(monitor_term),
        "the two producers write the identical frozen block"
    );
    let atoms = AtomTable::new();
    assert!(
        compare::eq(bare, monitor_term, &atoms),
        "HAZARD 2: the runtime's write_internal_ref block and a monitor reference block with the \
         same id compare =:="
    );
}

/// Pins the **mechanism** of the comparison, so the reason the hazard is real is explicit: two
/// references are `=:=` iff their payload words agree, and both producers pin `node`/`serial` to
/// `0`, leaving the id as the sole distinguishing word. A block that differed in `node` **would**
/// be distinguished — proof that varying a payload word is the only lever, and the lever the
/// id-only codec takes away (see the module docs).
#[test]
fn the_reference_payload_is_compared_word_for_word_leaving_only_the_id() {
    let mut heap = Heap::new(32).expect("test heap");
    let id = 3u64;

    // A reference from each producer, same id.
    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: four writable words, fully initialised by the writer; 3 fits a small.
    let built = unsafe { write_internal_ref(hp, id) }.expect("the id fits a small");
    let mut allocator = RefAllocator::new();
    let mut reference = allocator.next();
    while reference.id() < id {
        reference = allocator.next();
    }
    let mreffed = reference.to_eterm(&mut heap).expect("the monitor ref fits");

    // Both producers pin node (word 1) and serial (word 2) to 0. (`signed_value` is safe — it reads
    // a word's tagged value; the raw block reads already happened, guarded, in `ref_words`.)
    let built_words = ref_words(built);
    let mreffed_words = ref_words(mreffed);
    assert_eq!(built_words[0], mreffed_words[0], "the header words agree");
    assert_eq!(
        signed_value(built_words[1]),
        0,
        "writer: node is the local node (0)"
    );
    assert_eq!(
        signed_value(built_words[2]),
        0,
        "writer: one id space in M1"
    );
    assert_eq!(
        signed_value(mreffed_words[1]),
        0,
        "to_eterm: node is the local node (0)"
    );
    assert_eq!(
        signed_value(mreffed_words[2]),
        0,
        "to_eterm: one id space in M1"
    );

    // A fabricated block that differs ONLY in the node word is treated as a DIFFERENT reference —
    // which is why node/serial would be the lever to separate the two id spaces, and why pinning
    // them to 0 (this crate, `write_internal_ref`, and the id-only `beam_codec` decoder) forces the
    // collision onto the shared id.
    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: the four words are exclusively owned and written before the pointer escapes.
    unsafe {
        *p = make_header(3, REF_SUBTAG);
        *p.add(1) = make_small(1); // a non-zero node
        *p.add(2) = make_small(0);
        *p.add(3) = make_small(id as i64);
    }
    let different_node = make_boxed(p as *const Eterm);

    let atoms = AtomTable::new();
    assert!(
        !compare::eq(built, different_node, &atoms),
        "a ref block that differs only in the node word is a different reference — the payload \
         comparison would distinguish the two spaces if node/serial were free to vary"
    );
    assert!(
        compare::eq(built, mreffed, &atoms),
        "…but the two real producers both pin node/serial to 0, so only the shared id remains"
    );
}
