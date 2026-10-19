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

//! **PIN — the reference-id space: ONE shared, process-wide source of ids.**
//!
//! # Why one space
//!
//! A reference term here is the frozen on-heap block
//! `[make_header(3, REF_SUBTAG), node = 0, serial = 0, id]`, so a reference's **identity is the
//! `id` word alone** (`beam_term::compare::cmp_refs` compares the payload words, never the header
//! subtag, and `node`/`serial` are pinned to `0` by every writer — see
//! `the_reference_payload_is_compared_word_for_word_leaving_only_the_id`).
//!
//! Every local reference producer therefore has to draw its `id` from **one** counter, or two
//! producers can issue the same id and mint the very same term:
//!
//! * `beam_process`'s monitors and aliases (`ProcessTable`'s [`RefAllocator`] — the handle this
//!   file pins);
//! * `erlang:make_ref/0`, the NIF `make_ref`, the timer references, `seq_trace` references and NIF
//!   resource terms (`beam_rt`'s / `beam_abi`'s draws).
//!
//! The source of truth is `beam_common::next_ref_id()` — one process-wide `AtomicU64`, first value
//! `0`, monotone, total. `beam_process::RefAllocator` holds **no counter of its own**: it is a
//! zero-sized handle whose `next()` *is* that one draw. So:
//!
//! * two `RefAllocator`s (and two `ProcessTable`s, and two `Runtime`s in one OS process) can never
//!   hand out the same id — `two_allocators_never_repeat_an_id`;
//! * a monitor/alias reference can never collide with a bare `make_ref/0`-shaped reference —
//!   `a_table_draw_can_never_collide_with_a_bare_reference`;
//! * the draw is *exactly* the shared counter's next value — `a_table_draw_is_the_shared_spaces_next_id`
//!   and `an_allocator_draw_is_the_shared_spaces_next_id`.
//!
//! # Absolute ids are not reproducible
//!
//! Ids are now drawn from a counter shared by every test running in this binary, so an absolute
//! value (`id 0`, `id + 1`) is meaningless. The two exactness tests below therefore serialize on
//! [`TEST_LOCK`] and are the **only** tests in this file that draw references; every other test
//! asserts relative properties (strict increase, distinctness) or constructs an explicitly-valued
//! `MonitorRef` through `MonitorRef::from_eterm`.
//!
//! # No oracle cross-check
//!
//! The oracle has a single id space, so it cannot express "two producers, one id": the property
//! pinned here is the *absence* of a divergence, and it is unreachable from `erl -eval` (the oracle
//! exposes no way to observe its internal id counter across two producers). Deterministic — no
//! sleeps and no wall-clock bounds.

use std::sync::{Mutex, MutexGuard};

use beam_alloc::Heap;
use beam_common::next_ref_id;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    boxed_val, header_subtag, make_boxed, make_header, make_small, signed_value, write_internal_ref,
    AtomTable, Eterm, REF_SUBTAG,
};

/// Serializes this file's tests, so a draw from the process-wide reference-id space is
/// deterministic (`cargo test` runs the tests of one binary on parallel threads).
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Takes [`TEST_LOCK`], recovering from a poisoned mutex (a failed sibling test must not turn the
/// remaining tests into panics of their own).
fn serialize() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The four words of a reference term, read straight out of its on-heap block.
///
/// # Safety
///
/// Every term passed here is a live, four-word reference block built by one of the producers under
/// test (`MonitorRef::to_eterm` or `beam_term::boxed::write_internal_ref`).
fn ref_words(term: Eterm) -> [Eterm; 4] {
    // SAFETY: every argument is a live four-word ref block, so words 0..4 are in bounds.
    unsafe {
        let p = boxed_val(term);
        [*p, *p.add(1), *p.add(2), *p.add(3)]
    }
}

/// A `ProcessTable` with the canonical signal atoms and no local node — enough to mint a monitor,
/// an alias or a bare reference.
fn table() -> ProcessTable {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    ProcessTable::new(atoms)
}

/// **The shared source, exactly.** A table's draw is the shared counter's next value: after the
/// draw, one `beam_common::next_ref_id()` is precisely `id + 1`.
///
/// This is the property that makes the space real rather than merely "monotone": a private
/// per-instance counter would answer `id + 1` only by coincidence, and could not stay in step with
/// a draw from a *different* producer (the second half of the test draws one through the plain
/// [`RefAllocator`] handle and checks the same relation).
#[test]
fn a_table_draw_is_the_shared_spaces_next_id() {
    let _guard = serialize();
    let mut table = table();
    let owner = table.spawn_default().expect("an alias owner");

    // A monitor reference (`erlang:monitor/2`'s draw path).
    let target = table.spawn_default().expect("a monitor target");
    let monitor = table
        .monitor(owner, target, MonitorKind::Process)
        .expect("both processes are live");
    assert_eq!(
        next_ref_id(),
        monitor.id() + 1,
        "a monitor reference is the shared space's next id"
    );

    // An alias reference (`erlang:alias/0,1`'s draw path).
    let alias = table
        .create_alias(owner, UnaliasMode::ExplicitUnalias)
        .expect("the owner is live");
    assert_eq!(
        next_ref_id(),
        alias.id() + 1,
        "an alias reference is the shared space's next id"
    );

    // The table's bare mint (the shape a host uses for `make_ref/0`/timer/`seq_trace`).
    let bare = table.next_reference();
    assert_eq!(
        next_ref_id(),
        bare.id() + 1,
        "the table's bare mint is the shared space's next id"
    );
}

/// The same exactness for the plain [`RefAllocator`] handle that `beam_rt`'s `Runtime::refs`
/// holds — proving the two spellings are one counter, not two counters that happen to advance
/// together.
#[test]
fn an_allocator_draw_is_the_shared_spaces_next_id() {
    let _guard = serialize();
    let mut refs = RefAllocator::new();
    let draw = refs.next();
    assert_eq!(
        next_ref_id(),
        draw.id() + 1,
        "RefAllocator::next() IS beam_common::next_ref_id(), seen through a reference handle"
    );
}

/// **Two producers, no collision.** Two independent [`RefAllocator`]s hand out disjoint,
/// strictly-increasing ids — so two `ProcessTable`s (or a table and `beam_rt`'s
/// `Runtime::refs`) can never repeat an id, which is what closed the old HAZARD-2 collision.
#[test]
fn two_allocators_never_repeat_an_id() {
    let _guard = serialize();
    let mut a = RefAllocator::new();
    let mut b = RefAllocator::new();
    let a0 = a.next();
    let b0 = b.next();
    assert_ne!(a0, b0, "one space: the second handle does not restart at 0");
    assert!(b0.id() > a0.id(), "the draws interleave in one sequence");
    // …and it stays that way: every later draw is larger than every earlier one.
    let mut previous = b0.id();
    for _ in 0..8 {
        for reference in [a.next(), b.next()] {
            assert!(
                reference.id() > previous,
                "{} must exceed {previous}",
                reference.id()
            );
            previous = reference.id();
        }
    }
}

/// **The divergence that HAZARD 2 recorded is unrepresentable.** A `ProcessTable`-minted alias
/// reference and an *independent* allocator's (the shape of `beam_rt`'s `Runtime::refs`)
/// `make_ref/0`-shaped reference are now distinct ids, so their frozen blocks differ in the id word
/// — the sole distinguishing word — and they never compare `=:=`.
#[test]
fn a_table_draw_can_never_collide_with_a_bare_reference() {
    let _guard = serialize();
    let mut table = table();
    let owner = table.spawn_default().expect("an alias owner");
    let alias = table
        .create_alias(owner, UnaliasMode::ExplicitUnalias)
        .expect("the owner is live");

    // An INDEPENDENT allocator — the shape of `beam_rt`'s `Runtime::refs` (fed by `make_ref/0`).
    let bare = RefAllocator::new().next();
    assert_ne!(
        bare.id(),
        alias.id(),
        "one shared space: a bare reference can never reuse an alias's id"
    );
    assert!(bare.id() > alias.id(), "and it is drawn strictly later");

    // Both render through the very same `MonitorRef::to_eterm`; only the id word may differ, and
    // it does.
    let mut heap = Heap::new(16).expect("test heap");
    let alias_term = alias.to_eterm(&mut heap).expect("the alias ref fits");
    let bare_term = bare.to_eterm(&mut heap).expect("the bare ref fits");
    assert_ne!(
        ref_words(alias_term),
        ref_words(bare_term),
        "the two references are different four-word blocks"
    );
    // …and decode back to different `MonitorRef`s.
    assert_ne!(
        MonitorRef::from_eterm(alias_term).expect("an alias produces a reference term"),
        MonitorRef::from_eterm(bare_term).expect("a bare ref produces a reference term"),
    );
    // The observable consequence: they are not `=:=` (nor arithmetically `==`).
    let atoms = AtomTable::new();
    assert!(
        !compare::eq(alias_term, bare_term, &atoms),
        "a bare make_ref/0-shaped reference and an alias reference are different references"
    );
    assert!(!compare::cmp_arith(alias_term, bare_term, &atoms).is_eq());
}

/// The second half of the old HAZARD-2 pair: the runtime's own
/// `beam_term::boxed::write_internal_ref` block (`beam_bifs::make_ref_thing`'s shape) is the same
/// term as a [`MonitorRef::to_eterm`] block **iff** the id is explicitly equal — which no two
/// draws can be, since there is one space. The equality is therefore only reachable when a test
/// constructs the id explicitly, and this test pins exactly that: the *layout* is shared and
/// id-keyed, while the *ids* are handed out once.
#[test]
fn a_runtime_minted_block_equals_a_monitor_block_only_at_an_explicitly_shared_id() {
    let _guard = serialize();
    let mut heap = Heap::new(16).expect("test heap");

    // A real draw: its id, rendered the canonical writer's way.
    let draw = RefAllocator::new().next();
    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: `hp` owns exactly four words, which `write_internal_ref` fully initialises; the id
    // is a small (this test asserts that below).
    let bare = unsafe { write_internal_ref(hp, draw.id()) }.expect("the id fits a small");

    let monitor_term = draw.to_eterm(&mut heap).expect("the monitor ref fits");
    assert_eq!(
        ref_words(bare),
        ref_words(monitor_term),
        "the two producers write the identical frozen block for one id"
    );
    let atoms = AtomTable::new();
    assert!(
        compare::eq(bare, monitor_term, &atoms),
        "…so equal ids are equal terms — the reason ids must never repeat"
    );

    // The *ids*, however, never repeat: a second draw is a different block, hence a different
    // reference, even though the two producers agree on the layout.
    let other = RefAllocator::new()
        .next()
        .to_eterm(&mut heap)
        .expect("the second ref fits");
    assert_ne!(
        ref_words(bare),
        ref_words(other),
        "a later draw is a different block"
    );
    assert!(!compare::eq(bare, other, &atoms));
}

/// Pins the **mechanism** the whole file rests on: two references are `=:=` iff their payload words
/// agree, and every producer pins `node`/`serial` to `0`, leaving the id as the sole distinguishing
/// word. A block that differed in `node` **would** be distinguished — which is why the id space has
/// to be unique, and why the one shared counter is the fix (a payload field is not free to vary:
/// `beam_codec`'s internal-reference codec is id-only and rebuilds through `write_internal_ref`,
/// which zeroes both words).
#[test]
fn the_reference_payload_is_compared_word_for_word_leaving_only_the_id() {
    let _guard = serialize();
    let mut heap = Heap::new(32).expect("test heap");

    // A reference from each producer, at an explicitly shared id (only a test can do this — the
    // allocators themselves can no longer produce two equal ids).
    let draw = RefAllocator::new().next();
    let id = draw.id();

    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: four writable words, fully initialised by the writer; `id` is a small (it came out of
    // a draw, and `monitor_ref_to_eterm`-style encodes are checked at the encoder).
    let built = unsafe { write_internal_ref(hp, id) }.expect("the id fits a small");
    let mreffed = draw.to_eterm(&mut heap).expect("the monitor ref fits");

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
    assert_eq!(signed_value(built_words[2]), 0, "writer: serial is 0 in M1");
    assert_eq!(
        signed_value(mreffed_words[1]),
        0,
        "to_eterm: node is the local node (0)"
    );
    assert_eq!(signed_value(mreffed_words[2]), 0, "to_eterm: serial is 0 in M1");
    assert_eq!(
        signed_value(mreffed_words[3]),
        signed_value(built_words[3]),
        "and the id word is the whole difference a producer can make"
    );

    // A fabricated block that differs ONLY in the node word is treated as a DIFFERENT reference —
    // which is why node/serial *would* be a lever to separate two id spaces, and why pinning them
    // to 0 (this crate, `write_internal_ref`, and the id-only `beam_codec` decoder) makes id
    // uniqueness the only honest guarantee.
    // SAFETY: `alloc_words` returned four exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(4) }.expect("four words");
    // SAFETY: the four words are exclusively owned and written before the pointer escapes.
    unsafe {
        *p = make_header(3, REF_SUBTAG);
        *p.add(1) = make_small(1); // a non-zero node
        *p.add(2) = make_small(0);
        *p.add(3) = mreffed_words[3];
    }
    let different_node = make_boxed(p as *const Eterm);

    let atoms = AtomTable::new();
    assert!(
        !compare::eq(built, different_node, &atoms),
        "a ref block that differs only in the node word is a different reference"
    );
    assert!(
        compare::eq(built, mreffed, &atoms),
        "…but the two real producers both pin node/serial to 0, so only the id remains"
    );
}

/// The encoded id word of a real draw round-trips, and the check that the id is a small integer is
/// still the encoder's (a refusal past `MAX_SMALL`, never a truncated or substituted id) — the one
/// contract the shared space did **not** change.
#[test]
fn a_real_draw_encodes_and_decodes_byte_exactly() {
    let _guard = serialize();
    let mut heap = Heap::new(8).expect("test heap");
    let reference = RefAllocator::new().next();
    let term = reference.to_eterm(&mut heap).expect("a draw encodes");
    assert_eq!(
        MonitorRef::from_eterm(term).expect("and decodes"),
        reference,
        "the reference round-trips through its own term"
    );
    // SAFETY: `term` is a live, four-word block written by `to_eterm`, so words 0..4 are in bounds.
    unsafe {
        assert_eq!(header_subtag(*boxed_val(term)), REF_SUBTAG as u32);
        assert_eq!(
            signed_value(*boxed_val(term).add(3)),
            reference.id() as i64,
            "the id word holds this reference's own id"
        );
    }
}
