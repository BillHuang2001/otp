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

//! The **GC strategy dispatch on the production `Process` path**.
//!
//! `Process` owns a [`beam_gc::AnyCollector`], so the mature generation a real (runtime-spawned)
//! process collects with is the one its heap policy **resolves to**: the automatic rule of
//! [`beam_gc::selection`] picks the mark-region (GenImmix) space at/above
//! [`IMMIX_BREAK_EVEN_WORDS`] and the copying space below it — and a default policy is below it, so
//! the default keeps the copying path exactly as before.
//!
//! # Why this file exists
//!
//! Before the dispatch, `Process.collector` was the concrete `beam_gc::Collector`, so the question
//! "which mature strategy does this process collect with?" could not even be *asked* of a process:
//! there was no strategy to select. [`a_runtime_spawned_process_selects_the_strategy_its_policy_implies`]
//! is the assertion with no pre-change counterpart — it proves the production spawn path
//! (`ProcessTable::spawn(policy)` → `Process::with_policy` → `AnyCollector::new`) really lands the
//! strategy the policy implies **on the process**, not merely on a standalone collector built in a
//! unit test.
//!
//! The other tests pin the two halves of "no behaviour change":
//! [`the_default_process_keeps_the_pre_change_collection_cadence`] pins the default's
//! `CollectionKind` sequence, and
//! [`the_selected_strategy_collects_through_the_process`] /
//! [`an_immix_process_survives_a_fullsweep_through_the_process`] pin that whichever strategy was
//! selected collects **through the process root set**: the live x-register, stack and mailbox roots
//! survive (structurally) a minor sequence and a fullsweep, the Immix fullsweep included.

use beam_gc::{CollectionKind, HeapPolicy, MatureKind, IMMIX_BREAK_EVEN_WORDS};
use beam_process::prelude::*;
use beam_term::prelude::{list_val, make_small, tuple_element, tuple_size, AtomTable, NIL};

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A policy whose mature generation sits exactly at the automatic rule's break-even, i.e. the
/// smallest policy that resolves to [`MatureKind::Immix`].
///
/// Everything else is the default policy, so the *only* difference from a default process is the
/// mature generation's configured size — precisely what the rule reads.
fn immix_policy() -> HeapPolicy {
    HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
        ..HeapPolicy::default()
    }
}

/// Plants three live roots in `process`: a 2-tuple in `x(0)`, a cons on the stack and a small tuple
/// in the mailbox — the young heap's three root regions.
///
/// The roots are asserted after every collection (they are relocated, never re-created), so plant
/// them exactly once.
fn plant_roots(process: &mut Process) {
    let register_term = process
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the register tuple fits the nursery");
    {
        let registers = process.registers_mut();
        registers
            .x_mut()
            .set_live(1)
            .expect("1 <= MAX_REG, so the register is live");
        registers.x_mut().set(0, register_term).expect("0 < live");
    }

    let stack_term = process
        .heap_mut()
        .cons(make_small(3), NIL)
        .expect("the stack cons fits the nursery");
    process.registers_mut().stack_mut().push(stack_term);

    let mail_term = process
        .heap_mut()
        .tuple(&[make_small(5), make_small(6)])
        .expect("the mailbox tuple fits the nursery");
    process
        .deliver_simple(ProcessId::new(999, 0).to_eterm(), mail_term)
        .expect("the mailbox payload fits");
}

/// Asserts the three planted roots are still there and structurally intact.
///
/// Structural (`tuple_size`/`tuple_element`/`list_val`), never pointer identity: a collection is
/// expected to have **moved** them, so only their contents may be compared.
fn assert_roots_survive(process: &Process) {
    let register_term = process
        .registers()
        .x()
        .get(0)
        .expect("the register root is still live");
    // SAFETY: the live 2-tuple the register holds (relocated by the collection, never freed);
    // slots 0..2 are in bounds.
    unsafe {
        assert_eq!(tuple_size(register_term), 2);
        assert_eq!(tuple_element(register_term, 0), make_small(1));
        assert_eq!(tuple_element(register_term, 1), make_small(2));
    }

    let stack_term = process
        .registers()
        .stack()
        .as_slice()
        .first()
        .copied()
        .expect("the stack root is still there");
    // SAFETY: the live cons cell `[3 | NIL]` the stack holds (relocated); its two words are in
    // bounds.
    unsafe {
        let cell = list_val(stack_term);
        assert_eq!(*cell, make_small(3));
        assert_eq!(*cell.add(1), NIL);
    }

    assert_eq!(
        process.mailbox().len(),
        1,
        "the mailbox payload survived the collection"
    );
    let payload = process
        .mailbox()
        .peek()
        .expect("one message is queued")
        .payload();
    // SAFETY: the live 2-tuple the message payload holds (relocated); slots 0..2 are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 2);
        assert_eq!(tuple_element(payload, 0), make_small(5));
        assert_eq!(tuple_element(payload, 1), make_small(6));
    }
}

/// **The dispatch assertion a pre-change process could not express.** A process spawned through the
/// real `ProcessTable` path collects with the mature strategy its policy **resolves to**: at/above
/// [`IMMIX_BREAK_EVEN_WORDS`] the mark-region space, below it the copying space.
#[test]
fn a_runtime_spawned_process_selects_the_strategy_its_policy_implies() {
    let mut table = table();
    assert_eq!(IMMIX_BREAK_EVEN_WORDS, 2_048, "the pinned rule's break-even");

    // The automatic rule's Immix side, through the production spawn entry point.
    let immix_pid = table
        .spawn(immix_policy())
        .expect("the pid and spaces allocate");
    let immix = table.get(immix_pid).expect("the process is live");
    assert_eq!(
        immix.collector().kind(),
        MatureKind::Immix,
        "a mature generation at the break-even resolves to the mark-region strategy"
    );
    // ... and the source is the size-driven rule, not an explicit pin.
    assert_eq!(
        immix.collector().mature_choice().source,
        beam_gc::MatureSource::Automatic,
        "the strategy came from the automatic rule"
    );

    // The copying side, one word below the break-even, through the same entry point.
    let copying_policy = HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS - 1,
        ..HeapPolicy::default()
    };
    let copying_pid = table
        .spawn(copying_policy)
        .expect("the pid and spaces allocate");
    assert_eq!(
        table
            .get(copying_pid)
            .expect("the process is live")
            .collector()
            .kind(),
        MatureKind::Copying,
        "one word below the break-even keeps the copying strategy"
    );

    // The default policy is below the break-even, so a default spawn keeps the copying path too —
    // the dispatching field changes nothing for it.
    let default_pid = table.spawn_default().expect("the default process allocates");
    let default_process = table.get(default_pid).expect("the process is live");
    assert_eq!(
        default_process.collector().kind(),
        MatureKind::Copying
    );
    assert_eq!(
        default_process.collector().policy(),
        &HeapPolicy::default(),
        "the default policy is handed to the dispatcher verbatim"
    );
}

/// **No behaviour change for the default.** A default process still reports the copying strategy
/// and produces the collection-kind sequence it produced before the dispatch: with the default
/// `fullsweep_after = 65_535` no cadence fires and the small live set always fits the mature space,
/// so every `collect()` is a **minor** — the pre-change cadence.
#[test]
fn the_default_process_keeps_the_pre_change_collection_cadence() {
    let mut table = table();
    let pid = table.spawn_default().expect("the default process allocates");
    plant_roots(table.get_mut(pid).expect("the process is live"));

    let process = table.get(pid).expect("the process is live");
    assert_eq!(process.collector().kind(), MatureKind::Copying);
    assert_eq!(
        process.collector().policy().fullsweep_after,
        65_535,
        "the default fullsweep cadence (so no fullsweep fires here)"
    );
    assert_eq!(process.collector().minor_gcs(), 0);

    // The observed sequence, asserted literally: four minors in a row. This is the sequence a
    // pre-change (concrete-`Collector`) default process produced for the same four calls, because
    // the default policy is below the break-even and therefore still resolves to `Copying`.
    let mut kinds = Vec::new();
    for _ in 0..4 {
        kinds.push(
            table
                .get_mut(pid)
                .expect("the process is live")
                .collect()
                .expect("the collection succeeds")
                .kind,
        );
        assert_roots_survive(table.get(pid).expect("the process is live"));
    }
    assert_eq!(
        kinds,
        vec![
            CollectionKind::Minor,
            CollectionKind::Minor,
            CollectionKind::Minor,
            CollectionKind::Minor
        ]
    );

    let process = table.get(pid).expect("the process is live");
    assert_eq!(process.collector().minor_gcs(), 4, "four minors recorded");
    assert_eq!(process.collector().stats().major_collections, 0);
    assert_eq!(process.collector().stats().minor_collections, 4);

    // The growth seam keeps the default's cadence (it collects, and that collection is a minor).
    let grown = table
        .get_mut(pid)
        .expect("the process is live")
        .ensure_capacity(1)
        .expect("a one-word request is met");
    assert_eq!(grown.kind, CollectionKind::Minor);
    assert_roots_survive(table.get(pid).expect("the process is live"));
}

/// **The selected strategy really collects through the process.** With live x-register, stack and
/// mailbox roots planted, both a default (copying) and an Immix-configured process relocate those
/// roots across a plain-collection sequence and the growth seam.
#[test]
fn the_selected_strategy_collects_through_the_process() {
    let mut table = table();
    let copying = table.spawn_default().expect("the default process allocates");
    let immix = table.spawn(immix_policy()).expect("the pid and spaces allocate");

    assert_eq!(
        table.get(copying).expect("live").collector().kind(),
        MatureKind::Copying
    );
    assert_eq!(
        table.get(immix).expect("live").collector().kind(),
        MatureKind::Immix
    );

    for pid in [copying, immix] {
        plant_roots(table.get_mut(pid).expect("the process is live"));
        assert_roots_survive(table.get(pid).expect("the process is live"));

        // Three plain collections (a minor, or a fullsweep if the strategy's own rule asks for
        // one), each over the whole process root set.
        for _ in 0..3 {
            table
                .get_mut(pid)
                .expect("the process is live")
                .collect()
                .expect("the collection succeeds through the dispatcher");
            assert_roots_survive(table.get(pid).expect("the process is live"));
        }

        // The on-demand growth seam (collect + grow) goes through the same selected strategy.
        table
            .get_mut(pid)
            .expect("the process is live")
            .ensure_capacity(1)
            .expect("the growth seam succeeds through the dispatcher");
        assert_roots_survive(table.get(pid).expect("the process is live"));

        // `collect_if_needed` runs a collection only when the nursery cannot satisfy the request;
        // the roots survive either way.
        table
            .get_mut(pid)
            .expect("the process is live")
            .collect_if_needed(1)
            .expect("the admission query succeeds");
        assert_roots_survive(table.get(pid).expect("the process is live"));

        // A further delivery after collection still lands (the copy path uses the young space of
        // whichever strategy was selected).
        let second = table
            .get_mut(pid)
            .expect("the process is live")
            .heap_mut()
            .cons(make_small(7), NIL)
            .expect("the second payload fits");
        table
            .get_mut(pid)
            .expect("the process is live")
            .deliver_simple(ProcessId::new(998, 0).to_eterm(), second)
            .expect("a delivery after a collection succeeds");
        assert_eq!(
            table.get(pid).expect("the process is live").mailbox().len(),
            2,
            "both messages are queued"
        );
    }

    // Each process really ran its own strategy's collection through the process.
    for pid in [copying, immix] {
        let stats = table.get(pid).expect("the process is live").collector().stats();
        assert!(
            stats.major_collections + stats.minor_collections >= 1,
            "a collection ran through the process: {stats:?}"
        );
    }
}

/// **An Immix fullsweep through the process.** `fullsweep_after = 0` makes every `Process::collect`
/// a fullsweep, and the policy still resolves to the mark-region strategy (the rule reads the
/// mature *size*, not the cadence) — so the process's own `collect()` drives the GenImmix
/// `collect_major`, whose in-place reclaim/relocation must keep the process roots alive.
#[test]
fn an_immix_process_survives_a_fullsweep_through_the_process() {
    let mut table = table();
    let fullsweep_policy = HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
        fullsweep_after: 0,
        ..HeapPolicy::default()
    };
    let pid = table
        .spawn(fullsweep_policy)
        .expect("the pid and spaces allocate");
    assert_eq!(
        table.get(pid).expect("the process is live").collector().kind(),
        MatureKind::Immix,
        "the cadence does not affect the strategy the rule resolves to"
    );

    plant_roots(table.get_mut(pid).expect("the process is live"));

    // A `fullsweep_after` of 0 is due immediately, so each call is a fullsweep — on the mark-region
    // strategy, through the process root set.
    for _ in 0..3 {
        let collection = table
            .get_mut(pid)
            .expect("the process is live")
            .collect()
            .expect("the Immix fullsweep succeeds through the process");
        assert_eq!(collection.kind, CollectionKind::Major);
        assert_roots_survive(table.get(pid).expect("the process is live"));
    }

    let stats = table.get(pid).expect("the process is live").collector().stats();
    assert_eq!(stats.major_collections, 3, "three Immix fullsweeps ran");
    assert_eq!(stats.minor_collections, 0);
    assert_eq!(
        table.get(pid).expect("the process is live").collector().minor_gcs(),
        0,
        "a fullsweep resets the minor count (and `fullsweep_after = 0` never runs a minor)"
    );
}
