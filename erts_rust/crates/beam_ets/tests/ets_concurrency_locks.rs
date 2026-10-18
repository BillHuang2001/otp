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

//! The **coarse-lock correctness property** of [`SharedTable`] under real
//! multi-thread stress.
//!
//! `SharedTable` is an `Arc<RwLock<Table>>`: many readers, one exclusive writer,
//! and every check-then-act sequence runs *inside* one write guard. That single
//! coarse lock is what makes the advisory `{write_concurrency, _}` modes safe —
//! fine-grained locking can only change throughput, never results (`src/kind.rs`,
//! `src/table/shared.rs`).
//!
//! `tests/ets_concurrency.rs` already covers a global-lock independence check,
//! serial equivalence of a moved table, an 8-thread `insert_new` winner count, an
//! 8×500 `update_counter` triangular-sum witness and disjoint-key
//! `select_replace`. This suite adds the **stress + mutation-verified** set:
//!
//! 1. [`concurrent_counter_increments_total_exactly_n_times_m`] — 8 threads × 1000
//!    contended `update_counter` calls on one key; the final value is exactly
//!    `T × M` (a single lost or duplicated increment is a different number).
//! 2. [`concurrent_insert_new_of_one_key_has_exactly_one_winner`] — 16 threads all
//!    `insert_new` the *same* key: exactly one `true`, fifteen `false`, one object.
//! 3. [`concurrent_select_replace_and_update_element_never_lose_or_duplicate`] — 8
//!    threads × 200 iterations alternating `select_replace` and `update_element` on
//!    one shared object while each thread also upserts a key of its own: afterwards
//!    there are `1 + T` objects, the shared key is present **exactly once**, and
//!    every object is a well-formed `{Key, Value}`.
//! 4. [`a_poisoned_lock_degrades_to_the_typed_error`] — a writer that panics under
//!    the guard poisons the lock, and the next `read()`/`write()` reports the typed
//!    `unsupported_ets: table lock poisoned` raise — never a wrong value and never
//!    a panic.
//!
//! # Mutation verification (recorded in the crate `CONTEXT.md`)
//!
//! These tests were checked against a deliberately broken `SharedTable`: with the
//! closure handed a table that is *not* the shared one (so nothing it does is
//! serialized against the shared state), tests 1 and 2 fail with the exact messages
//! quoted in `CONTEXT.md`. They are therefore sensitive to the property they claim,
//! not merely to "the code compiled".
//!
//! The guard-scoped views make the *literal* "lost update" mutation unavailable in
//! safe Rust: [`TableWrite`] can only be built from an `RwLockWriteGuard`, so there
//! is no way to hand a closure a mutable table without holding the lock. The
//! read-then-write split that *is* expressible (take the guard, drop it, re-acquire
//! before running the closure) is semantically inert — pinned by the experiment
//! recorded in `CONTEXT.md`.
//!
//! The views are `!Send` by design, so each thread creates its own [`TableWrite`] /
//! [`TableRead`] from its cloned [`SharedTable`].

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use beam_common::{BeamError, ExitReason};
use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions, TableTerm};
use beam_term::prelude::*;
use common::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fresh, `public` table of `kind` wrapped for sharing across threads.
fn shared_table(kind: TableKind) -> SharedTable {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_access(Access::Public);
    SharedTable::new(Table::new(TableId::next(), opts).expect("a fresh table is constructible"))
}

/// Insert `object` into `shared` as the owner would.
fn put(shared: &SharedTable, object: Eterm, atoms: &AtomTable) {
    shared
        .with_write(|view| view.insert(object, atoms))
        .expect("no poisoned lock")
        .expect("a valid object inserts");
}

/// Element `index` of a stored object, as an owned small integer.
///
/// Reading it *inside* the guard is what keeps nothing table-owned escaping it.
fn element(object: &TableTerm<'_>, index: usize) -> Option<i64> {
    object.tuple_element(index).and_then(|element| element.as_small_int())
}

/// The `(key, value)` fields of the single object stored under `key`, read out as
/// owned values under one read guard.
///
/// `None` fields mean the stored term is not a well-formed 2-tuple of small
/// integers — which is what the stress test's "never torn" assertion catches.
fn stored_pairs(shared: &SharedTable, key: i64, atoms: &AtomTable) -> Vec<(Option<i64>, Option<i64>)> {
    shared
        .with_read(|view| {
            view.lookup(make_small(key), atoms)
                .iter()
                .map(|object| (element(object, 0), element(object, 1)))
                .collect()
        })
        .expect("no poisoned lock")
}

// ---------------------------------------------------------------------------
// 1. T threads × M increments of one key: the total is exactly T×M
// ---------------------------------------------------------------------------

/// Threads contending on the one counter key.
const COUNTER_THREADS: usize = 8;
/// Increments each of them performs.
const COUNTER_PER_THREAD: usize = 1000;
/// The contended counter key.
const COUNTER_KEY: i64 = 1;

/// `T` threads × `M` contended `update_counter` calls total exactly `T×M`.
///
/// The witness is arithmetic, not statistical: with a lost update the total is some
/// smaller number, and with a duplicated read-add-write it is larger. Only a
/// sequence that is atomic with respect to every other writer lands on `T×M`.
#[test]
fn concurrent_counter_increments_total_exactly_n_times_m() {
    let shared = shared_table(TableKind::Set);
    let atoms = AtomTable::new();
    let mut seed = Heap::new(8);
    put(&shared, object(&mut seed, COUNTER_KEY, 0), &atoms);

    std::thread::scope(|scope| {
        for _ in 0..COUNTER_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                // The `set` interior's default `update_counter` rebuilds the object
                // on this heap, so size it for every increment this worker runs.
                let mut out = rebuild_heap_words(COUNTER_PER_THREAD * 4 + 64);
                for _ in 0..COUNTER_PER_THREAD {
                    let value = shared
                        .with_write(|view| {
                            view.update_counter(make_small(COUNTER_KEY), make_small(1), atoms, &mut out)
                        })
                        .expect("no poisoned lock")
                        .expect("the counter object is present");
                    assert!(is_small(value), "an increment returns an integer");
                }
            });
        }
    });

    let total = shared
        .with_read(|view| {
            view.lookup_element(make_small(COUNTER_KEY), 2, &atoms)
                .expect("the counter object is present")
                .as_small_int()
                .expect("the counter stays a small integer")
        })
        .expect("no poisoned lock");
    let operations = (COUNTER_THREADS * COUNTER_PER_THREAD) as i64;
    assert_eq!(
        total, operations,
        "no increment may be lost and none duplicated: {COUNTER_THREADS} threads × \
         {COUNTER_PER_THREAD} increments must total {operations}, found {total}"
    );
}

// ---------------------------------------------------------------------------
// 2. T threads racing insert_new on one key: exactly one winner
// ---------------------------------------------------------------------------

/// Threads racing on the one contended key.
const WINNER_THREADS: usize = 16;
/// The contended key (distinct from every other test's key, so no fixture is shared).
const WINNER_KEY: i64 = 777_001;

/// All `T` threads `insert_new` the *same* key: exactly one wins, the table holds
/// one object, and its value is one of the offered ones.
#[test]
fn concurrent_insert_new_of_one_key_has_exactly_one_winner() {
    let shared = shared_table(TableKind::Set);
    let atoms = AtomTable::new();
    let winners = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for thread in 0..WINNER_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            let winners = &winners;
            scope.spawn(move || {
                let mut heap = Heap::new(8);
                let candidate = object(&mut heap, WINNER_KEY, thread as i64 + 1);
                let inserted = shared
                    .with_write(|view| view.insert_new(candidate, atoms))
                    .expect("no poisoned lock")
                    .expect("an insert_new of a supported object succeeds");
                if inserted {
                    winners.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let winners = winners.load(Ordering::Relaxed);
    assert_eq!(
        winners, 1,
        "exactly one of the {WINNER_THREADS} contended insert_new calls may win, {winners} did"
    );
    let size = shared.with_read(|view| view.size()).expect("no poisoned lock");
    assert_eq!(size, 1, "the loser's object must not be stored");
    let pairs = stored_pairs(&shared, WINNER_KEY, &atoms);
    assert_eq!(pairs.len(), 1, "the key is present exactly once");
    let value = pairs[0].1.expect("the stored value is a small integer");
    assert!(
        (1..=WINNER_THREADS as i64).contains(&value),
        "the stored value is one of the offered ones, found {value}"
    );
}

// ---------------------------------------------------------------------------
// 3. Alternating select_replace / update_element on one object, plus own inserts
// ---------------------------------------------------------------------------

/// Threads alternating the two rewrites.
const REWRITE_THREADS: usize = 8;
/// Iterations each of them performs.
const REWRITE_ITERATIONS: usize = 200;
/// The one object every thread rewrites.
const SHARED_KEY: i64 = 555;
/// The first per-thread key (each thread's own, never contended).
const OWN_KEY_BASE: i64 = 1000;

/// Alternating `select_replace` (key-keeping) and `update_element [ {2, V} ]` on one
/// shared object, concurrently with per-thread upserts of that thread's own key,
/// neither loses nor duplicates an object: the table ends with `1 + T` objects, the
/// shared key is present exactly once, and every object is a well-formed
/// `{Key, Value}` — never two objects for a key, never none, never a torn tuple.
#[test]
fn concurrent_select_replace_and_update_element_never_lose_or_duplicate() {
    let mut atoms = AtomTable::new();
    let mut source = Heap::new(1024);
    // The only body `ets:select_replace/2` accepts: it keeps the object's key and
    // rewrites element 2 to `0` (`common::key_keeping_spec`).
    let spec = key_keeping_spec(&mut source, &mut atoms);

    let shared = shared_table(TableKind::Set);
    {
        let mut seed = Heap::new(8);
        put(&shared, object(&mut seed, SHARED_KEY, 1), &atoms);
    }

    std::thread::scope(|scope| {
        for thread in 0..REWRITE_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            let spec = &spec;
            scope.spawn(move || {
                let own_key = OWN_KEY_BASE + thread as i64;
                let mut heap = Heap::new(8);
                let mut out = rebuild_heap_words(REWRITE_ITERATIONS * 16 + 64);
                for iteration in 0..REWRITE_ITERATIONS {
                    let value = (thread * REWRITE_ITERATIONS + iteration) as i64 + 2;
                    if iteration % 2 == 0 {
                        let present = shared
                            .with_write(|view| {
                                view.update_element(
                                    make_small(SHARED_KEY),
                                    &[(2, make_small(value))],
                                    atoms,
                                    &mut out,
                                )
                            })
                            .expect("no poisoned lock")
                            .expect("a valid element update");
                        assert!(present, "the shared object is never absent");
                    } else {
                        let replaced = shared
                            .with_write(|view| view.select_replace(spec, atoms, &mut out))
                            .expect("no poisoned lock")
                            .expect("the key-keeping spec applies");
                        // The spec's head matches every object, so it rewrites at
                        // least the always-present shared one; how many of the
                        // per-thread keys exist yet is up to the schedule, which is
                        // exactly why only a lower bound is asserted here (the exact
                        // final population is asserted below).
                        assert!(
                            replaced >= 1,
                            "the key-keeping spec must rewrite at least the shared object, \
                             replaced {replaced}"
                        );
                    }
                    // …and this thread's own, never-contended key.
                    heap.reset();
                    put(&shared, object(&mut heap, own_key, value), atoms);
                }
            });
        }
    });

    let size = shared.with_read(|view| view.size()).expect("no poisoned lock");
    assert_eq!(
        size,
        1 + REWRITE_THREADS,
        "one shared object plus one per thread, never two, never none"
    );

    let shared_objects = stored_pairs(&shared, SHARED_KEY, &atoms);
    assert_eq!(
        shared_objects.len(),
        1,
        "the shared key must be present exactly once, found {} objects",
        shared_objects.len()
    );
    assert_eq!(shared_objects[0].0, Some(SHARED_KEY), "the key element is intact");
    assert!(
        shared_objects[0].1.is_some(),
        "the value element is a well-formed small integer, never a torn tuple"
    );

    for thread in 0..REWRITE_THREADS {
        let own_key = OWN_KEY_BASE + thread as i64;
        let objects = stored_pairs(&shared, own_key, &atoms);
        assert_eq!(
            objects.len(),
            1,
            "thread {thread}'s own key {own_key} must be present exactly once"
        );
        assert_eq!(objects[0].0, Some(own_key));
        assert!(objects[0].1.is_some(), "…with a well-formed value");
    }
}

// ---------------------------------------------------------------------------
// 4. A poisoned lock is a typed error, not a wrong answer and not a panic
// ---------------------------------------------------------------------------

/// The typed raise a poisoned lock produces, exactly as `src/table/shared.rs` builds
/// it (`crate::error::unsupported("table lock poisoned")`).
const POISONED_REASON: &str = "unsupported_ets: table lock poisoned";

/// A writer that panics while holding the guard poisons the lock; the next reader
/// and the next writer both get the typed `unsupported_ets: table lock poisoned`
/// raise — never a table whose invariants may be broken, never a panic.
#[test]
fn a_poisoned_lock_degrades_to_the_typed_error() {
    let shared = shared_table(TableKind::Set);
    let poisoner = shared.clone();
    let panicked = thread::spawn(move || {
        // The panic happens *inside* the write guard, which is what poisons it.
        let _ = poisoner.with_write(|_| panic!("poison the lock"));
    })
    .join();
    assert!(panicked.is_err(), "the writer thread must have panicked");

    let read_error = shared
        .read()
        .err()
        .expect("a poisoned read must raise rather than hand out the table");
    let write_error = shared
        .write()
        .err()
        .expect("a poisoned write must raise rather than hand out the table");

    for error in [read_error, write_error] {
        match error {
            BeamError::Raise(raise) => match raise.reason {
                ExitReason::Atom(name) => assert_eq!(
                    name.as_str(),
                    POISONED_REASON,
                    "the poisoned lock's reason atom names the failure"
                ),
                other => panic!("expected the poisoned reason atom, got {other:?}"),
            },
            other => panic!(
                "a poisoned lock must be the typed `unsupported_ets` raise, got {other:?}"
            ),
        }
    }

    // The closure forms refuse too — no path reaches the table.
    assert!(
        shared.with_read(|view| view.size()).is_err(),
        "`with_read` must refuse a poisoned table"
    );
    assert!(
        shared.with_write(|view| view.size()).is_err(),
        "`with_write` must refuse a poisoned table"
    );
}
