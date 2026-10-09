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

//! Thread-safety of a shared ETS table: `Table: Send + Sync`, the [`SharedTable`]
//! lock wrapper, and the "no lost updates" guarantee under concurrent writers — the
//! concurrency evidence for the crate's shared-table path.
//!
//! Four layers of evidence:
//!
//! 1. **Compile-time**: `Table`, `SharedTable` and `TermStore` are asserted
//!    `Send + Sync` ([`table_and_shared_table_are_send_and_sync`]), so a regression in
//!    the crate's hand-written `unsafe impl`s (`src/store/mod.rs`) becomes a compile
//!    error.
//! 2. **Exact concurrent results.** Every concurrency test asserts the *exact* state
//!    a serial execution of the same operation scripts would produce — a lost update
//!    or a torn entry would change a total, never merely make a test "less likely to
//!    pass". Ordinal/probabilistic assertions are deliberately never the evidence:
//!    * [`two_interleaved_accessors_yield_the_serial_result`] runs two independent
//!      [`SharedTable`] accessors lockstep behind a [`std::sync::Barrier`]: script A
//!      inserts and deletes its own disjoint key range, script B `insert_new`s its own
//!      range, and both increment one shared counter.
//!    * [`concurrent_counter_increments_lose_no_updates`] drives `N` OS threads × `M`
//!      `ets:update_counter`-equivalent increments of one key, each increment inside
//!      **one** `with_write` guard, and asserts the total is exactly `N × M`.
//!    * [`readers_never_observe_an_inconsistent_table`] and
//!      [`readers_see_only_consistent_states_under_writer_churn`] prove a locked read
//!      only ever observes a self-consistent *snapshot*, even while a writer is
//!      inserting/replacing/deleting.
//! 3. **The oracle for every result is ordinary single-threaded `ets`**: the point of
//!    the suite is that concurrency changes nothing observable, so the expected totals
//!    are the serial ones.
//! 4. **Live-oracle cross-check** ([`concurrent_counter_total_agrees_with_the_live_oracle`]):
//!    the *same* `N` workers × `M` increments workload runs under the real `erl`, and
//!    the Rust total must equal the oracle's total for the same `N` and `M`. The
//!    oracle is **required by default**, so a missing Erlang fails this check; set
//!    `BEAM_ETS_ALLOW_NO_ORACLE=1` to opt out explicitly.
//!
//! # A read result is only valid while its read guard is held
//!
//! [`Table::lookup`]/[`Table::tab2list`] return `Eterm`s that **alias table-owned
//! storage** — the A.1 copy boundary is on the *insert* side, not the read side. So a
//! read result is only sound while the caller still holds the read guard that produced
//! it: once the guard is released a writer may remove that object, reclaim its words
//! and have a later `insert` reuse them, at which point the same word reads as a
//! *different*, well-formed object. (Real `ets` copies the result into the calling
//! process's heap and so has no such constraint.) Every check in the reader tests below
//! therefore runs *inside* the `with_read` closure, and only owned integers escape.

mod common;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use beam_alloc::Heap as RebuildHeap;
use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions, TableTerm, TermStore};
use beam_term::prelude::*;

use common::oracle;
use common::{object, rebuild_heap_words};

// ---------------------------------------------------------------------------
// Fixtures and parameters
// ---------------------------------------------------------------------------

/// Workers in the confined-counter stress tests.
const INCR_THREADS: usize = 8;
/// Increments each counter worker performs.
const INCR_PER_THREAD: usize = 500;
/// The one key every counter worker contends on.
const COUNTER_KEY: i64 = 1;

/// Rounds each interleaved script runs.
const ROUNDS: usize = 60;
/// Keys each interleaved script touches per round.
const PER_ROUND: usize = 5;
/// Keys script A owns (a `set` keeps at most one object per key).
const SCRIPT_KEYS: usize = ROUNDS * PER_ROUND;
/// Script B's key base — disjoint from script A's `0 .. SCRIPT_KEYS`.
const B_KEY_BASE: i64 = 100_000;
/// The key of the counter **both** scripts share.
const SHARED_COUNTER_KEY: i64 = 1_000_000;
/// Script A deletes every `A_DELETED_EVERY`-th key it inserted.
const A_DELETED_EVERY: i64 = 3;

/// Keys the churn writer cycles through.
const CHURN_KEYS: i64 = 16;
/// Churn rounds the writer performs.
const CHURN_ROUNDS: i64 = 3_000;
/// Reads the churn reader always performs, even after the writer has stopped, so a
/// starved reader can never turn the test into a vacuous pass.
const CHURN_MIN_READS: usize = 50;

/// Compile-time proof that a type may cross thread boundaries and be shared behind a
/// reference (`std::thread::scope` requires exactly this).
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn table_and_shared_table_are_send_and_sync() {
    assert_send_sync::<Table>();
    assert_send_sync::<SharedTable>();
    // The term arena behind the table's copy boundary carries the crate's
    // hand-written `unsafe impl Send`/`unsafe impl Sync` (`src/store/mod.rs`);
    // asserting them here makes a regression in those impls a compile error.
    assert_send_sync::<TermStore>();
}

/// A still-open table (the tests drive several requesters explicitly).
fn open_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_access(Access::Public);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A `beam_term::heap::Heap` with room for one worker's `INCR_PER_THREAD`
/// three-word tuples.
fn worker_heap() -> Heap {
    Heap::new(INCR_PER_THREAD * 4 + 16)
}

/// Assert `object` is a well-formed `{Key, Value}` 2-tuple of small integers and
/// return `(key, value)`.
///
/// This is the torn-entry detector of the reader tests: a half-written object (a
/// missing/duplicated element, a stale pointer, an out-of-band word) fails here.
///
/// `object` is read through the view's lifetime-tagged [`TableTerm`], so every
/// element access stays inside the read guard the term was read under; the returned
/// pair is owned, so it — and only it — may outlive that guard.
fn as_pair(object: &TableTerm<'_>) -> (i64, i64) {
    assert_eq!(
        object.tuple_arity(),
        Some(2),
        "every stored object is a 2-tuple"
    );
    // Extract the two elements as *owned* small integers: reading them yields further
    // tagged terms, which must not escape the guard, and the tests only ever compare
    // their numeric values.
    let key = object
        .tuple_element(0)
        .expect("a 2-tuple has a key element")
        .as_small_int()
        .expect("every stored element is a small integer");
    let value = object
        .tuple_element(1)
        .expect("a 2-tuple has a value element")
        .as_small_int()
        .expect("every stored element is a small integer");
    (key, value)
}

/// A shared `set` holding exactly the counter object `{COUNTER_KEY, 0}`.
fn shared_counter_table() -> SharedTable {
    let shared = SharedTable::new(open_table(TableKind::Set));
    // The counter object is built on a `beam_term` test arena; `insert` deep-copies it
    // into table-owned storage (the A.1 copy boundary), so the arena need not outlive
    // the call.
    let mut heap = Heap::new(8);
    let counter = object(&mut heap, COUNTER_KEY, 0);
    let atoms = AtomTable::new();
    shared
        .with_write(|table| table.insert(counter, &atoms))
        .expect("an unpoisoned table lock")
        .expect("a valid counter object inserts");
    shared
}

/// Drive `threads` × `increments` `ets:update_counter`-equivalent `+1` increments of
/// `COUNTER_KEY` through `shared` — one increment per `with_write` guard, so the
/// read-add-write is atomic — and return `(final counter total, sum of the values the
/// increments returned)`.
///
/// The second element is an independent witness of "no lost update": each increment
/// returns the counter's *new* value, so with no lost update the returned values are
/// exactly `1..=threads*increments` and their sum is the triangular number.
fn drive_counter_increments(
    shared: &SharedTable,
    atoms: &AtomTable,
    threads: usize,
    increments: usize,
) -> (i64, i64) {
    let returned_sum = AtomicI64::new(0);
    let performed = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let shared = shared.clone();
            let returned_sum = &returned_sum;
            let performed = &performed;
            scope.spawn(move || {
                // The `set` interior's default `update_counter` rebuilds the whole
                // object on this heap, so size it for every increment this thread runs.
                let mut out = rebuild_heap_words(increments * 4 + 64);
                for _ in 0..increments {
                    let new_value = shared
                        .with_write(|table| {
                            table.update_counter(
                                make_small(COUNTER_KEY),
                                make_small(1),
                                atoms,
                                &mut out,
                            )
                        })
                        .expect("an unpoisoned table lock")
                        .expect("the counter object is present");
                    assert!(is_small(new_value), "an increment returns an integer");
                    returned_sum.fetch_add(signed_value(new_value), Ordering::Relaxed);
                    performed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(
        performed.load(Ordering::Relaxed),
        threads * increments,
        "every increment ran"
    );
    let total = shared
        .with_read(|table| {
            // Read the counter into an owned integer *inside* the guard: the term a
            // read returns is tagged to the closure-local view (and aliases
            // table-owned storage), so a later writer could reclaim and reuse it.
            let counter = table
                .lookup_element(make_small(COUNTER_KEY), 2, atoms)
                .expect("the counter object is present");
            counter
                .as_small_int()
                .expect("the counter stays a small integer")
        })
        .expect("an unpoisoned table lock");
    (total, returned_sum.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Existing evidence: disjoint-key writers and a locked-read invariant
// ---------------------------------------------------------------------------

#[test]
fn concurrent_writers_lose_no_updates() {
    let shared = SharedTable::new(open_table(TableKind::Set));
    // The test terms are small-integer tuples, so `eq`/`term_hash` never consult an
    // atom table; each worker can therefore own an empty one without any risk of
    // disagreeing about an atom's index. (A shared `AtomTable` would need its own
    // lock, which is the runtime's concern — see the module docs of
    // `src/table/shared.rs`.)
    let atoms = AtomTable::new();

    std::thread::scope(|scope| {
        for thread in 0..INCR_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let mut heap = worker_heap();
                for i in 0..INCR_PER_THREAD {
                    let key = (thread * INCR_PER_THREAD + i) as i64;
                    let obj = object(&mut heap, key, key);
                    shared
                        .with_write(|table| table.insert(obj, atoms))
                        .expect("an unpoisoned table lock")
                        .expect("a valid object inserts");
                }
            });
        }
    });

    assert_eq!(
        shared.with_read(|table| table.size()).unwrap(),
        INCR_THREADS * INCR_PER_THREAD,
        "every writer's disjoint keys must all be present — a lost update \
         would leave the table short"
    );
    // The same total is visible through an independent enumeration.
    assert_eq!(
        shared.with_read(|table| table.tab2list().len()).unwrap(),
        INCR_THREADS * INCR_PER_THREAD
    );
}

/// Readers running against a writer never observe a broken table: each read sees
/// a self-consistent size (`tab2list().len() == size()`), and the sizes only ever
/// grow until all writers are done.
#[test]
fn readers_never_observe_an_inconsistent_table() {
    let shared = SharedTable::new(open_table(TableKind::Set));
    let atoms = AtomTable::new();
    let reads = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for thread in 0..INCR_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let mut heap = worker_heap();
                for i in 0..INCR_PER_THREAD {
                    let key = (thread * INCR_PER_THREAD + i) as i64;
                    let obj = object(&mut heap, key, key);
                    shared
                        .with_write(|table| table.insert(obj, atoms))
                        .unwrap()
                        .unwrap();
                }
            });
        }
        for _ in 0..4 {
            let shared = shared.clone();
            let reads = &reads;
            scope.spawn(move || {
                for _ in 0..INCR_PER_THREAD {
                    // The read guard makes the two observations atomic with respect
                    // to every writer, so they can never disagree.
                    let (size, listed) = shared
                        .with_read(|table| (table.size(), table.tab2list().len()))
                        .unwrap();
                    assert_eq!(size, listed, "a locked read is internally consistent");
                    assert!(size <= INCR_THREADS * INCR_PER_THREAD);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(reads.load(Ordering::Relaxed), 4 * INCR_PER_THREAD);
    assert_eq!(
        shared.with_read(|table| table.size()).unwrap(),
        INCR_THREADS * INCR_PER_THREAD
    );
}

/// Access enforcement still works per requester when the table is shared: an
/// exclusive write lock plus a `protected` table lets exactly one requester write.
#[test]
fn a_protected_table_admits_only_the_owner_writer_under_concurrency() {
    use beam_ets::OwnerId;

    const N: i64 = 64;
    let owner = OwnerId::new(1);
    let mut opts = TableOptions::new();
    opts.set_access(Access::Protected);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner));
    let shared = SharedTable::new(table);
    let atoms = AtomTable::new();

    // Drive `N` inserts as `requester`, returning how many the table admitted.
    let drive = |requester: OwnerId| {
        let shared = shared.clone();
        let atoms = &atoms;
        move || {
            let mut heap = Heap::new(N as usize * 4 + 16);
            let mut admitted = 0usize;
            for key in 0..N {
                let obj = object(&mut heap, key, key);
                let outcome = shared.with_write(|table| {
                    let mut view = table.as_requester(Some(requester));
                    view.insert(obj, atoms)
                });
                match outcome.unwrap() {
                    Ok(()) => admitted += 1,
                    Err(beam_common::BeamError::BadArg) => {}
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
            admitted
        }
    };

    let (owner_admitted, stranger_admitted) = std::thread::scope(|scope| {
        let owner_writer = scope.spawn(drive(owner));
        let stranger_writer = scope.spawn(drive(OwnerId::new(2)));
        (
            owner_writer.join().expect("the owner thread did not panic"),
            stranger_writer
                .join()
                .expect("the stranger thread did not panic"),
        )
    });

    assert_eq!(owner_admitted, N as usize, "the owner may write");
    assert_eq!(stranger_admitted, 0, "a stranger may not write");
    assert_eq!(shared.with_read(|table| table.size()).unwrap(), N as usize);
}

/// A `Table` may simply be *moved* to another thread (it is `Send`), which is the
/// other half of sharing a table across processes.
#[test]
fn a_table_can_be_moved_to_another_thread() {
    let mut table = open_table(TableKind::OrderedSet);
    let atoms = AtomTable::new();
    {
        let mut heap = Heap::new(64);
        table.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    }
    let handle = std::thread::spawn(move || {
        let mut heap = Heap::new(64);
        table.insert(object(&mut heap, 2, 20), &atoms).unwrap();
        table.tab2list().len()
    });
    assert_eq!(handle.join().expect("the worker thread did not panic"), 2);
}

// ---------------------------------------------------------------------------
// Two independent accessors, interleaved on purpose
// ---------------------------------------------------------------------------

/// Flatten a `SharedTable` outcome (`Result<Result<T>>` — the lock, then the
/// operation) into one fallible value with a printable message.
///
/// The scripts below never *panic* mid-round: a failing script must keep reaching
/// the round barrier so its peer can still finish, and report only once both have
/// rendezvoused for the last time.
fn flatten<T: core::fmt::Debug>(
    outcome: beam_common::Result<beam_common::Result<T>>,
) -> Result<T, String> {
    match outcome {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("the operation failed: {error:?}")),
        Err(error) => Err(format!("the table lock is unusable: {error:?}")),
    }
}

/// One round of script A: insert `{k, k}` for `PER_ROUND` keys of its own disjoint
/// range and delete every `A_DELETED_EVERY`-th of them, then increment the shared
/// counter. Never panics (see [`flatten`]).
fn script_a_round(
    shared: &SharedTable,
    atoms: &AtomTable,
    round: usize,
    heap: &mut Heap,
    out: &mut RebuildHeap,
    increments: &AtomicUsize,
) -> Result<(), String> {
    // Reset the source arena once per round: every object built in the previous round
    // was deep-copied into table-owned storage by `insert`, so no live term points
    // into this arena.
    heap.reset();
    for i in 0..PER_ROUND {
        let key = (round * PER_ROUND + i) as i64;
        let obj = object(heap, key, key);
        flatten(shared.with_write(|table| table.insert(obj, atoms)))?;
        if key % A_DELETED_EVERY == 0 {
            flatten(shared.with_write(|table| table.delete(make_small(key), atoms)))?;
        }
    }
    let new_value = flatten(shared.with_write(|table| {
        table.update_counter(make_small(SHARED_COUNTER_KEY), make_small(1), atoms, out)
    }))?;
    if !is_small(new_value) {
        return Err("an increment must return an integer".to_string());
    }
    increments.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// One round of script B: `insert_new` `{k, k}` for `PER_ROUND` keys of its own
/// disjoint range, `insert_new` each of them a second time (which must be refused),
/// then increment the shared counter. Never panics (see [`flatten`]).
fn script_b_round(
    shared: &SharedTable,
    atoms: &AtomTable,
    round: usize,
    heap: &mut Heap,
    out: &mut RebuildHeap,
    inserted: &AtomicUsize,
    rejected: &AtomicUsize,
    increments: &AtomicUsize,
) -> Result<(), String> {
    heap.reset();
    for i in 0..PER_ROUND {
        let key = B_KEY_BASE + (round * PER_ROUND + i) as i64;
        let obj = object(heap, key, key);
        let added = flatten(shared.with_write(|table| table.insert_new(obj, atoms)))?;
        if added {
            inserted.fetch_add(1, Ordering::Relaxed);
        }
        // A second `insert_new` for the same key is refused — under its own guard, so
        // nothing can slip between the call and the check — and must leave the stored
        // object alone.
        heap.reset();
        let duplicate = object(heap, key, -1);
        let again = flatten(shared.with_write(|table| table.insert_new(duplicate, atoms)))?;
        if again {
            return Err(format!("key {key} is already present, but `insert_new` re-added it"));
        }
        rejected.fetch_add(1, Ordering::Relaxed);
    }
    let new_value = flatten(shared.with_write(|table| {
        table.update_counter(make_small(SHARED_COUNTER_KEY), make_small(1), atoms, out)
    }))?;
    if !is_small(new_value) {
        return Err("an increment must return an integer".to_string());
    }
    increments.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Two independent [`SharedTable`] accessors sharing **one** table must leave
/// exactly the state a serial execution of their two scripts would leave.
///
/// Script A inserts `{k, k}` for every key of its own disjoint range `0..SCRIPT_KEYS`
/// and then deletes every `A_DELETED_EVERY`-th of them. Script B `insert_new`s `{k, k}`
/// for every key of its own disjoint range, and immediately `insert_new`s the *same*
/// key again with a different object — which must be refused without disturbing the
/// stored object. Both scripts then increment the **same** shared counter once per
/// round.
///
/// A [`std::sync::Barrier`] synchronises every round, so the two scripts' operations
/// genuinely interleave: after each barrier both threads run a whole round of
/// operations concurrently. Both scripts reach the barrier for all `ROUNDS` rounds
/// even if one of them has already failed, and only report afterwards — a failure
/// must never leave its peer blocked on the barrier.
#[test]
fn two_interleaved_accessors_yield_the_serial_result() {
    let shared = SharedTable::new(open_table(TableKind::Set));
    let atoms = AtomTable::new();
    // The shared counter object, inserted up front by this (owner) thread.
    {
        let mut heap = Heap::new(8);
        let counter = object(&mut heap, SHARED_COUNTER_KEY, 0);
        shared
            .with_write(|table| table.insert(counter, &atoms))
            .expect("an unpoisoned table lock")
            .expect("a valid counter object inserts");
    }

    let barrier = Barrier::new(2);
    let a_increments = AtomicUsize::new(0);
    let b_increments = AtomicUsize::new(0);
    let b_inserted = AtomicUsize::new(0);
    let b_rejected = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        // --- Script A: insert/delete over its own disjoint key range -------------
        {
            let shared = shared.clone();
            let atoms = &atoms;
            let barrier = &barrier;
            let a_increments = &a_increments;
            scope.spawn(move || {
                let mut heap = Heap::new(PER_ROUND * 4 + 8);
                let mut out = rebuild_heap_words(ROUNDS * 8 + 64);
                let mut failure: Option<String> = None;
                for round in 0..ROUNDS {
                    barrier.wait();
                    if failure.is_some() {
                        // Keep rendezvousing so the peer can finish its own script.
                        continue;
                    }
                    failure = script_a_round(
                        &shared,
                        atoms,
                        round,
                        &mut heap,
                        &mut out,
                        a_increments,
                    )
                    .err();
                }
                if let Some(message) = failure {
                    panic!("script A failed: {message}");
                }
            });
        }
        // --- Script B: `insert_new` over its own disjoint range + the counter ----
        {
            let shared = shared.clone();
            let atoms = &atoms;
            let barrier = &barrier;
            let b_increments = &b_increments;
            let b_inserted = &b_inserted;
            let b_rejected = &b_rejected;
            scope.spawn(move || {
                let mut heap = Heap::new(PER_ROUND * 4 + 8);
                let mut out = rebuild_heap_words(ROUNDS * 8 + 64);
                let mut failure: Option<String> = None;
                for round in 0..ROUNDS {
                    barrier.wait();
                    if failure.is_some() {
                        continue;
                    }
                    failure = script_b_round(
                        &shared,
                        atoms,
                        round,
                        &mut heap,
                        &mut out,
                        b_inserted,
                        b_rejected,
                        b_increments,
                    )
                    .err();
                }
                if let Some(message) = failure {
                    panic!("script B failed: {message}");
                }
            });
        }
    });

    // --- the observable state equals the serial composition of the two scripts ---
    assert_eq!(b_inserted.load(Ordering::Relaxed), SCRIPT_KEYS);
    assert_eq!(b_rejected.load(Ordering::Relaxed), SCRIPT_KEYS);
    assert_eq!(a_increments.load(Ordering::Relaxed), ROUNDS);
    assert_eq!(b_increments.load(Ordering::Relaxed), ROUNDS);

    let (size, listed_len, a_pairs, b_pairs, counter) = shared
        .with_read(|table| {
            // Every check below runs *inside* the read guard: a term a read returns is
            // tagged to the closure-local view and aliases table-owned storage, so a
            // result is only valid while the guard is still held (a later writer may
            // remove the object and have its words reclaimed and reused). The owned
            // `(i64, i64)` pairs are what outlives the guard.
            let a_pairs: Vec<Option<(i64, i64)>> = (0..SCRIPT_KEYS as i64)
                .map(|key| single(&table.lookup(make_small(key), &atoms)))
                .collect();
            let b_pairs: Vec<Option<(i64, i64)>> = (0..SCRIPT_KEYS as i64)
                .map(|i| single(&table.lookup(make_small(B_KEY_BASE + i), &atoms)))
                .collect();
            let counter = table
                .lookup_element(make_small(SHARED_COUNTER_KEY), 2, &atoms)
                .expect("the shared counter is present")
                .as_small_int()
                .expect("the shared counter is a small integer");
            (table.size(), table.tab2list().len(), a_pairs, b_pairs, counter)
        })
        .expect("an unpoisoned table lock");

    assert_eq!(size, listed_len, "size and tab2list must agree");

    // Script A: its keys survive exactly when it did not delete them.
    let mut a_survivors = 0usize;
    for (index, hit) in a_pairs.iter().enumerate() {
        let key = index as i64;
        if key % A_DELETED_EVERY == 0 {
            assert!(hit.is_none(), "script A deleted key {key}");
            continue;
        }
        a_survivors += 1;
        assert_eq!(*hit, Some((key, key)), "key {key}'s object is untouched");
    }
    assert!(a_survivors > 0 && a_survivors < SCRIPT_KEYS, "script A both inserted and deleted");

    // Script B: every key of its range, each holding the object of the *first* insert.
    for (index, hit) in b_pairs.iter().enumerate() {
        let key = B_KEY_BASE + index as i64;
        assert_eq!(
            *hit,
            Some((key, key)),
            "the refused duplicate `insert_new` must not have replaced key {key}'s object"
        );
    }

    // The shared counter: exactly one increment per script per round.
    assert_eq!(
        counter,
        2 * ROUNDS as i64,
        "the shared counter must total exactly one increment per script per round"
    );

    assert_eq!(
        size,
        a_survivors + SCRIPT_KEYS + 1,
        "the table holds script A's survivors, script B's keys and the counter — \
         exactly the serial composition"
    );
    // Every stored object is well-formed: no torn entry survived the interleaving.
    shared
        .with_read(|table| {
            for object in &table.tab2list() {
                as_pair(object);
            }
        })
        .expect("an unpoisoned table lock");
}

/// Validate a `lookup` result — a `set` keeps at most one object per key — and
/// return it as owned integers. **Must** be called while the table's read guard is
/// held (see the aliasing note in the reader-churn test); the returned pair is owned,
/// so it may outlive the guard.
fn single(hits: &[TableTerm<'_>]) -> Option<(i64, i64)> {
    assert!(hits.len() <= 1, "a `set` keeps at most one object per key");
    hits.first().map(|object| as_pair(object))
}

// ---------------------------------------------------------------------------
// No lost update: N threads × M increments of one key
// ---------------------------------------------------------------------------

/// `INCR_THREADS` × `INCR_PER_THREAD` increments of **one** key, each inside one
/// `with_write` guard, must total exactly `INCR_THREADS × INCR_PER_THREAD`.
///
/// The oracle is the serial total: `ets:update_counter` is atomic per call, so `N`
/// workers × `M` calls can only ever yield `N × M`, whatever the interleaving. Both
/// the final total and the sum of the values the increments *returned* (which must be
/// exactly `1..=N×M`) are asserted, so a lost update — or a duplicated read-add-write
/// — changes a number.
#[test]
fn concurrent_counter_increments_lose_no_updates() {
    let shared = shared_counter_table();
    let atoms = AtomTable::new();

    let (total, returned_sum) =
        drive_counter_increments(&shared, &atoms, INCR_THREADS, INCR_PER_THREAD);

    let expected = (INCR_THREADS * INCR_PER_THREAD) as i64;
    assert_eq!(
        total, expected,
        "{INCR_THREADS} threads × {INCR_PER_THREAD} increments must total exactly {expected}"
    );
    assert_eq!(
        returned_sum,
        expected * (expected + 1) / 2,
        "every increment must have returned a distinct new value 1..={expected}"
    );
}

// ---------------------------------------------------------------------------
// Reader consistency under writer churn
// ---------------------------------------------------------------------------

/// One writer inserting/replacing/deleting a rolling window of keys while a reader
/// repeatedly reads. The reader must never observe a torn state: within one read
/// guard, `size`, `tab2list` and the per-key `lookup`s must agree, and every object
/// must be a well-formed `{Key, Value}` pair whose value is `Key` or `Key + 1` (the
/// two objects the writer ever stores).
#[test]
fn readers_see_only_consistent_states_under_writer_churn() {
    let shared = SharedTable::new(open_table(TableKind::Set));
    let atoms = AtomTable::new();
    let start = Barrier::new(2);
    let done = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        // --- the writer: insert, replace, delete --------------------------------
        {
            let shared = shared.clone();
            let atoms = &atoms;
            let start = &start;
            let done = &done;
            scope.spawn(move || {
                let mut heap = Heap::new(8);
                start.wait();
                for round in 0..CHURN_ROUNDS {
                    let key = round % CHURN_KEYS;
                    // Reset the source arena: both objects below are deep-copied into
                    // table-owned storage by `insert` before the next reset.
                    heap.reset();
                    let first = object(&mut heap, key, key);
                    shared
                        .with_write(|table| table.insert(first, atoms))
                        .expect("an unpoisoned table lock")
                        .expect("a valid object inserts");
                    heap.reset();
                    let replacement = object(&mut heap, key, key + 1);
                    shared
                        .with_write(|table| table.insert(replacement, atoms))
                        .expect("an unpoisoned table lock")
                        .expect("a valid replacement inserts");
                    if round >= CHURN_KEYS {
                        shared
                            .with_write(|table| table.delete(make_small(key), atoms))
                            .expect("an unpoisoned table lock")
                            .expect("deleting a present key succeeds");
                    }
                }
                done.store(true, Ordering::Release);
            });
        }
        // --- the reader: every observation must be one consistent snapshot ------
        {
            let shared = shared.clone();
            let atoms = &atoms;
            let start = &start;
            let done = &done;
            let reads = &reads;
            scope.spawn(move || {
                start.wait();
                let mut performed = 0usize;
                loop {
                    // ONE read guard = ONE atomic snapshot of the whole table, and
                    // every check runs *inside* the guard. That is essential: an
                    // `Eterm` a read returns aliases table-owned storage, so once the
                    // guard is released a writer may remove the object, reclaim its
                    // words, and have a later `insert` reuse them — a result is only
                    // valid while the guard is still held.
                    shared
                        .with_read(|table| {
                            let size = table.size();
                            let listed = table.tab2list();
                            assert_eq!(size, listed.len(), "size and tab2list disagree");
                            assert!(size <= CHURN_KEYS as usize);

                            // Every enumerated object is a well-formed
                            // `{Key, Key|Key+1}` — the two objects the writer stores.
                            let mut listed_keys = Vec::with_capacity(listed.len());
                            for object in &listed {
                                let (key, value) = as_pair(object);
                                assert!(
                                    (0..CHURN_KEYS).contains(&key),
                                    "a key outside the churn window: {key}"
                                );
                                assert!(
                                    value == key || value == key + 1,
                                    "a torn object {{{key}, {value}}}"
                                );
                                listed_keys.push(key);
                            }
                            listed_keys.sort_unstable();

                            // The per-key probes are the same snapshot, so they must
                            // agree with the enumeration exactly — both on membership
                            // and on the object each key holds.
                            let mut probe_keys = Vec::with_capacity(listed.len());
                            for probe in 0..CHURN_KEYS {
                                let hits = table.lookup(make_small(probe), atoms);
                                assert!(
                                    hits.len() <= 1,
                                    "a set keeps at most one object per key, found {}",
                                    hits.len()
                                );
                                for object in &hits {
                                    let (key, value) = as_pair(object);
                                    assert_eq!(
                                        key, probe,
                                        "lookup({probe}) returned an object keyed {key}"
                                    );
                                    assert!(
                                        value == key || value == key + 1,
                                        "a torn object {{{key}, {value}}}"
                                    );
                                    probe_keys.push(key);
                                }
                            }
                            probe_keys.sort_unstable();
                            assert_eq!(
                                listed_keys, probe_keys,
                                "lookup and tab2list disagree about which keys are present"
                            );
                        })
                        .expect("an unpoisoned table lock");

                    performed += 1;
                    reads.fetch_add(1, Ordering::Relaxed);
                    if performed >= CHURN_MIN_READS && done.load(Ordering::Acquire) {
                        break;
                    }
                    // Give the writer a chance to run between reads on a busy host.
                    thread::yield_now();
                }
            });
        }
    });

    let performed = reads.load(Ordering::Relaxed);
    assert!(
        performed >= CHURN_MIN_READS,
        "the reader must have performed at least {CHURN_MIN_READS} consistent reads, did {performed}"
    );
    // The quiescent end state is consistent too, and every stored pair is well-formed.
    shared
        .with_read(|table| {
            let listed = table.tab2list();
            assert_eq!(table.size(), listed.len());
            for object in &listed {
                let (key, value) = as_pair(object);
                assert!(value == key || value == key + 1);
            }
        })
        .expect("an unpoisoned table lock");
}

// ---------------------------------------------------------------------------
// Live-oracle cross-check of the concurrent counter workload
// ---------------------------------------------------------------------------

/// Workers and increments-per-worker of the oracle cross-check workload.
const ORACLE_THREADS: usize = 8;
/// Increments each oracle worker performs (see [`ORACLE_THREADS`]).
const ORACLE_INCREMENTS: usize = 500;
/// Wall-clock bound for the oracle child, so a wedged oracle fails instead of hanging.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The embedded oracle program (an `escript`): it performs the *same* workload as
/// [`drive_counter_increments`] against the real `ets` — `N` processes each doing `M`
/// `ets:update_counter/3` increments of one key of one table, then read the total —
/// and prints it as `total -> <n>`.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main([NStr, MStr]) ->
    N = list_to_integer(NStr),
    M = list_to_integer(MStr),
    Tab = ets:new(shared_counter, [set, public]),
    ets:insert(Tab, {1, 0}),
    Parent = self(),
    Workers = [spawn_link(fun() -> increment(Tab, M), Parent ! {done, self()} end)
               || _ <- lists:seq(1, N)],
    [receive {done, Pid} -> ok end || Pid <- Workers],
    io:format("total -> ~p~n", [ets:lookup_element(Tab, 1, 2)]),
    ets:delete(Tab),
    ok.

increment(_Tab, 0) -> ok;
increment(Tab, Left) ->
    _ = ets:update_counter(Tab, 1, 1),
    increment(Tab, Left - 1).
"#;

/// The concurrent counter workload must produce the oracle's exact total.
#[test]
fn concurrent_counter_total_agrees_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "concurrent_counter_total_agrees_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let args = [ORACLE_THREADS.to_string(), ORACLE_INCREMENTS.to_string()];
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &args, "shared", ORACLE_TIMEOUT);
    let oracle_total = oracle::parse_total(&output.stdout).unwrap_or_else(|| {
        panic!(
            "the oracle printed no `total ->` line:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let shared = shared_counter_table();
    let atoms = AtomTable::new();
    let (rust_total, _) =
        drive_counter_increments(&shared, &atoms, ORACLE_THREADS, ORACLE_INCREMENTS);

    println!(
        "[shared] concurrent update_counter total: oracle={oracle_total} rust={rust_total} \
         ({ORACLE_THREADS} workers × {ORACLE_INCREMENTS} increments)"
    );
    assert_eq!(
        rust_total, oracle_total,
        "beam_ets and the live oracle must agree on the concurrent counter total"
    );
    assert_eq!(
        oracle_total,
        (ORACLE_THREADS * ORACLE_INCREMENTS) as i64,
        "the oracle's own total is the serial one"
    );
}

// The oracle plumbing itself — `resolve_escript`/`find_in_path`/`is_executable`,
// the bounded `escript` runner and the `total ->` parser — lives in
// `tests/common/oracle.rs`, shared with the crate's other oracle cross-checks.