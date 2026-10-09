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

//! Concurrency evidence for the shared-table path: **lock independence**,
//! **serial equivalence** and **no lost updates** on every check-then-act
//! operation.
//!
//! `tests/ets_shared.rs` proves the `Send`/`Sync` bounds, that a locked read is
//! atomic, and that disjoint-key writers and counter increments lose nothing under
//! the two interleaved accessor scripts. This suite completes that evidence with
//! four properties the earlier one does not pin:
//!
//! 1. **One lock per table, not one global lock**
//!    ([`independent_tables_do_not_share_one_global_lock`]). The test holds
//!    *exclusive* write guards on half of its tables and then proves a writer on
//!    each of the other half still acquires its own table's lock — and, as a
//!    non-vacuity check, that a writer on a table whose guard *is* held does block
//!    (and is released the moment that guard is dropped). A single shared global
//!    lock would time the independent writers out.
//! 2. **A moved/cloned handle is serial-equivalent**
//!    ([`a_moved_or_cloned_table_is_serial_equivalent`]). The same deterministic
//!    script — sharded across OS threads on one table, run whole on another —
//!    produces *byte-identical* final state.
//! 3. **No lost updates in any check-then-act sequence.** Each of
//!    [`concurrent_insert_new_of_one_key_admits_exactly_one_winner`] (`insert_new`
//!    of one key: exactly one winner),
//!    [`concurrent_insert_new_of_distinct_keys_stores_every_object`] (`insert_new`
//!    of distinct keys: all present),
//!    [`concurrent_update_counter_of_one_key_loses_no_updates`] (`update_counter`
//!    of one key: the exact total, witnessed independently by the triangular sum of
//!    the values the increments returned) and
//!    [`concurrent_select_replace_of_disjoint_keys_rewrites_every_object`]
//!    (`select_replace` of disjoint keys: every object rewritten exactly once)
//!    runs its whole check-then-act sequence inside **one** guard.
//! 4. **Live-oracle cross-check**
//!    ([`the_insert_new_winner_count_agrees_with_the_live_oracle`]): the *same*
//!    contended `insert_new` workload runs under the real `erl`, and the oracle's
//!    own winner count is one — so the Rust invariant is the oracle's invariant, not
//!    a local convention. The oracle is **required by default**, so a missing Erlang
//!    fails this check; set `BEAM_ETS_ALLOW_NO_ORACLE=1` to opt out explicitly.
//!
//! # Why every assertion is exact
//!
//! A lost update, a torn entry or a spuriously-won race changes a *total*, a *count*
//! or a *stored value* — never merely the odds that a test passes. So no assertion
//! here is ordinal or probabilistic: each names the exact number, sequence or table
//! rendering a serial execution of the same work would produce.

mod common;

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_ets::{
    compile, Access, MatchSpec, SharedTable, Table, TableId, TableKind, TableOptions, TableTerm,
    TableWrite,
};
use beam_term::prelude::*;

use common::oracle;
use common::{object, proper_list, rebuild_heap_words, tuple_of};

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// A fresh, publicly-accessible table of `kind`, wrapped for sharing.
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

/// Element `index` of a tagged object, as an owned small integer. Extracting the
/// number *inside* the guard is what keeps nothing table-owned escaping it.
fn element(object: &TableTerm<'_>, index: usize) -> i64 {
    object
        .tuple_element(index)
        .and_then(|element| element.as_small_int())
        .expect("every stored object is a 2-tuple of small integers")
}

/// The whole table as owned, sorted `(key, value)` pairs — the exact final-state
/// rendering every serial-equivalence check compares.
fn rendering(shared: &SharedTable) -> Vec<(i64, i64)> {
    let mut rows: Vec<(i64, i64)> = shared
        .with_read(|view| {
            view.tab2list()
                .iter()
                .map(|object| (element(object, 0), element(object, 1)))
                .collect()
        })
        .expect("no poisoned lock");
    rows.sort_unstable();
    rows
}

// ---------------------------------------------------------------------------
// 1. Independent tables do not share one global lock
// ---------------------------------------------------------------------------

/// Holding an exclusive guard on one table must not stop a writer on a *different*
/// table — and holding it must genuinely stop a writer on the *same* table.
///
/// The second half is the non-vacuity check: without it, "the other writers were
/// not blocked" would also hold if the guards had silently failed to lock anything.
#[test]
fn independent_tables_do_not_share_one_global_lock() {
    const TABLES: usize = 16;
    // Generous bound: a lock that is free is acquired in microseconds; this only
    // has to distinguish "acquired" from "blocked behind a global lock".
    const WAIT: Duration = Duration::from_secs(5);

    let tables: Vec<SharedTable> = (0..TABLES).map(|_| shared_table(TableKind::Set)).collect();
    let half = TABLES / 2;

    // Hold an exclusive write guard on the first half for the whole test body.
    let held: Vec<TableWrite<'_>> = tables[..half]
        .iter()
        .map(|table| table.write().expect("no poisoned lock"))
        .collect();

    // A writer on each of the other tables must still acquire its own lock. Each
    // worker runs on a *detached* thread, so a hypothetical global lock would make
    // `recv_timeout` fail the test rather than hang it.
    let (sender, receiver) = mpsc::channel::<usize>();
    let workers: Vec<thread::JoinHandle<()>> = (half..TABLES)
        .map(|index| {
            let table = tables[index].clone();
            let sender = sender.clone();
            thread::spawn(move || {
                let mut heap = Heap::new(8);
                let atoms = AtomTable::new();
                table
                    .with_write(|view| view.insert(object(&mut heap, index as i64, 1), &atoms))
                    .expect("no poisoned lock")
                    .expect("a valid object inserts");
                sender.send(index).expect("the receiver is alive");
            })
        })
        .collect();
    drop(sender);

    let mut signalled = Vec::with_capacity(TABLES - half);
    for _ in half..TABLES {
        signalled.push(receiver.recv_timeout(WAIT).expect(
            "an independent table's writer must not block behind another table's guard",
        ));
    }
    for worker in workers {
        worker.join().expect("an independent writer did not panic");
    }
    signalled.sort_unstable();
    assert_eq!(
        signalled,
        (half..TABLES).collect::<Vec<_>>(),
        "every independent table admitted exactly one writer"
    );

    // Non-vacuity: the held guards really are exclusive. This writer must *not*
    // get in while we hold `tables[0]`'s guard...
    let (blocked_tx, blocked_rx) = mpsc::channel::<()>();
    let contended = tables[0].clone();
    let blocked = thread::spawn(move || {
        let _guard = contended.write().expect("no poisoned lock");
        let _ = blocked_tx.send(());
    });
    assert!(
        blocked_rx.recv_timeout(Duration::from_millis(250)).is_err(),
        "a held write guard must exclude a second writer on the same table"
    );
    // ...and dropping it releases exactly that writer.
    drop(held);
    blocked_rx
        .recv_timeout(WAIT)
        .expect("dropping the guard must release the writer blocked on it");
    blocked.join().expect("the contended writer did not panic");
}

// ---------------------------------------------------------------------------
// 2. A moved/cloned handle is serial-equivalent
// ---------------------------------------------------------------------------

/// Keys the serial-equivalence script spans.
const WORK_KEYS: i64 = 500;
/// Shards the script is split across on the concurrent table.
const WORK_THREADS: usize = 8;

/// One deterministic work script over `keys`: upsert `{key, (key*7+1) % 1000}`, then
/// delete the keys divisible by 7. Applied to disjoint key sets it is
/// order-independent, so a sharded run must reproduce a whole run exactly.
fn run_script(shared: &SharedTable, atoms: &AtomTable, keys: Vec<i64>) {
    let mut heap = Heap::new(8);
    for key in &keys {
        // Reset the source arena: `insert` deep-copies the object into table-owned
        // storage before the next reset (the A.1 copy boundary).
        heap.reset();
        put(shared, object(&mut heap, *key, (*key * 7 + 1) % 1000), atoms);
    }
    for key in &keys {
        if key % 7 == 0 {
            shared
                .with_write(|view| view.delete(make_small(*key), atoms))
                .expect("no poisoned lock")
                .expect("deleting a present key succeeds");
        }
    }
}

/// The same script run serially and sharded across OS threads (each with its own
/// *moved* clone of one shared handle) leaves an identical final state.
#[test]
fn a_moved_or_cloned_table_is_serial_equivalent() {
    let atoms = AtomTable::new();
    let serial = shared_table(TableKind::Set);
    let concurrent = shared_table(TableKind::Set);

    run_script(&serial, &atoms, (0..WORK_KEYS).collect());

    std::thread::scope(|scope| {
        for shard in 0..WORK_THREADS {
            let handle = concurrent.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let keys: Vec<i64> = (0..WORK_KEYS)
                    .filter(|key| (*key as usize) % WORK_THREADS == shard)
                    .collect();
                run_script(&handle, atoms, keys);
            });
        }
    });

    // Identical final state, field for field...
    assert_eq!(
        rendering(&serial),
        rendering(&concurrent),
        "a sharded run must reproduce the serial run exactly"
    );
    assert_eq!(
        serial.with_read(|view| view.size()).unwrap(),
        concurrent.with_read(|view| view.size()).unwrap()
    );
    // ...and it is the state the script's own rules dictate.
    let expected: Vec<(i64, i64)> = (0..WORK_KEYS)
        .filter(|key| key % 7 != 0)
        .map(|key| (key, (key * 7 + 1) % 1000))
        .collect();
    assert_eq!(rendering(&concurrent), expected);
}

// ---------------------------------------------------------------------------
// 3. No lost updates in any check-then-act sequence
// ---------------------------------------------------------------------------

/// The one key every `insert_new`/counter worker contends on.
const CONTENDED_KEY: i64 = 1;

/// Every worker offers the *same* key a different value through `insert_new`: the
/// single guard around the check-then-act must admit exactly one, and the stored
/// object must be one of the offered ones — never a mix of two.
#[test]
fn concurrent_insert_new_of_one_key_admits_exactly_one_winner() {
    const THREADS: usize = 16;
    let atoms = AtomTable::new();
    let shared = shared_table(TableKind::Set);
    let winners = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            let winners = &winners;
            scope.spawn(move || {
                let mut heap = Heap::new(8);
                let candidate = object(&mut heap, CONTENDED_KEY, thread as i64);
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

    assert_eq!(
        winners.load(Ordering::Relaxed),
        1,
        "exactly one insert_new may win the check-then-act race"
    );
    assert_eq!(
        shared.with_read(|view| view.size()).unwrap(),
        1,
        "the contended key is present exactly once"
    );
    let stored = shared
        .with_read(|view| {
            view.lookup_element(make_small(CONTENDED_KEY), 2, &atoms)
                .expect("the winner's object is present")
                .as_small_int()
                .expect("the stored value stays a small integer")
        })
        .unwrap();
    assert!(
        (0..THREADS as i64).contains(&stored),
        "the stored object is one of the offered ones, found value {stored}"
    );
}

/// Sharded `insert_new` over **distinct** keys stores every object: no shard's
/// insertion is dropped, and every one of them reports success.
#[test]
fn concurrent_insert_new_of_distinct_keys_stores_every_object() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 128;

    let atoms = AtomTable::new();
    let shared = shared_table(TableKind::Set);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let mut heap = Heap::new(PER_THREAD * 4 + 16);
                for i in 0..PER_THREAD {
                    let key = (thread * PER_THREAD + i) as i64;
                    let inserted = shared
                        .with_write(|view| view.insert_new(object(&mut heap, key, key), atoms))
                        .expect("no poisoned lock")
                        .expect("an insert_new of a supported object succeeds");
                    assert!(inserted, "a distinct key's insert_new must succeed");
                }
            });
        }
    });

    assert_eq!(
        shared.with_read(|view| view.size()).unwrap(),
        THREADS * PER_THREAD,
        "every shard's objects are present"
    );
    assert_eq!(
        shared.with_read(|view| view.tab2list().len()).unwrap(),
        THREADS * PER_THREAD,
        "an independent enumeration agrees"
    );
    assert_eq!(rendering(&shared).len(), THREADS * PER_THREAD);
}

/// A shared `set` holding exactly `{CONTENDED_KEY, 0}`.
fn shared_counter_table() -> SharedTable {
    let shared = shared_table(TableKind::Set);
    let mut heap = Heap::new(8);
    let atoms = AtomTable::new();
    put(&shared, object(&mut heap, CONTENDED_KEY, 0), &atoms);
    shared
}

/// Drive `threads` × `increments` `+1` increments of `CONTENDED_KEY` — one
/// increment per guard, so the read-add-write is atomic — and return
/// `(final total, sum of the values the increments returned)`.
///
/// The second element is an independent witness of "no lost update": each increment
/// returns the counter's *new* value, so with nothing lost the returned values are
/// exactly `1..=threads*increments` and their sum is the triangular number.
fn drive_counter_increments(
    shared: &SharedTable,
    atoms: &AtomTable,
    threads: usize,
    increments: usize,
) -> (i64, i64) {
    let returned_sum = AtomicI64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let shared = shared.clone();
            let atoms = &atoms;
            let returned_sum = &returned_sum;
            scope.spawn(move || {
                // The `set` interior's default `update_counter` rebuilds the whole
                // object on this heap, so size it for every increment this worker runs.
                let mut out = rebuild_heap_words(increments * 4 + 64);
                for _ in 0..increments {
                    let new_value = shared
                        .with_write(|view| {
                            view.update_counter(
                                make_small(CONTENDED_KEY),
                                make_small(1),
                                atoms,
                                &mut out,
                            )
                        })
                        .expect("no poisoned lock")
                        .expect("the counter object is present");
                    assert!(is_small(new_value), "an increment returns an integer");
                    returned_sum.fetch_add(signed_value(new_value), Ordering::Relaxed);
                }
            });
        }
    });

    let total = shared
        .with_read(|view| {
            view.lookup_element(make_small(CONTENDED_KEY), 2, atoms)
                .expect("the counter object is present")
                .as_small_int()
                .expect("the counter stays a small integer")
        })
        .expect("no poisoned lock");
    (total, returned_sum.load(Ordering::Relaxed))
}

/// `threads` × `increments` contended `update_counter` calls lose no update: the
/// total is exact, and the returned values are exactly `1..=threads*increments`.
#[test]
fn concurrent_update_counter_of_one_key_loses_no_updates() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 500;

    let shared = shared_counter_table();
    let atoms = AtomTable::new();
    let (total, returned_sum) = drive_counter_increments(&shared, &atoms, THREADS, PER_THREAD);

    let operations = (THREADS * PER_THREAD) as i64;
    assert_eq!(total, operations, "no increment may be lost");
    assert_eq!(
        returned_sum,
        operations * (operations + 1) / 2,
        "the increments must have returned exactly 1..={operations}"
    );
}

/// Keys the `select_replace` workload spans.
const REPLACE_KEYS: usize = 256;
/// Shards the `select_replace` workload is split across.
const REPLACE_THREADS: usize = 8;

/// `{Head, Guards, Body}` — one match-spec clause tuple.
fn clause(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = proper_list(heap, guards);
    let body_list = proper_list(heap, body);
    tuple_of(heap, &[head, guard_list, body_list])
}

/// The `{{ … }}` body construction form: a one-element tuple wrapping a tuple.
fn construct(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let inner = tuple_of(heap, elements);
    tuple_of(heap, &[inner])
}

/// A one-clause `select_replace` spec that rewrites the object keyed `key` to
/// `{key, value}`. The head matches the key by its *literal* term, so each key's
/// spec is disjoint from every other key's.
fn replace_spec(heap: &mut Heap, wildcard: Eterm, key: i64, value: i64) -> Eterm {
    let head = tuple_of(heap, &[make_small(key), wildcard]);
    let body = construct(heap, &[make_small(key), make_small(value)]);
    let clause = clause(heap, head, &[], &[body]);
    proper_list(heap, &[clause])
}

/// Sharded `select_replace` over **disjoint** keys rewrites every object exactly
/// once, and the final values are exactly the ones each spec constructed.
#[test]
fn concurrent_select_replace_of_disjoint_keys_rewrites_every_object() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));

    // One compiled spec per key, built and compiled up front: compiling needs
    // `&mut AtomTable`, which cannot be shared with the workers.
    let mut source = Heap::new(REPLACE_KEYS * 32 + 64);
    let specs: Vec<MatchSpec> = (0..REPLACE_KEYS as i64)
        .map(|key| {
            let term = replace_spec(&mut source, wildcard, key, key * 1000 + 1);
            compile(term, &mut atoms).expect("the replace spec compiles")
        })
        .collect();

    let shared = shared_table(TableKind::Set);
    {
        let mut heap = Heap::new(8);
        for key in 0..REPLACE_KEYS as i64 {
            // Reset the source arena: `insert` deep-copies the object first.
            heap.reset();
            put(&shared, object(&mut heap, key, 0), &atoms);
        }
    }

    let replaced = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for shard in 0..REPLACE_THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            let specs = &specs;
            let replaced = &replaced;
            scope.spawn(move || {
                let mut out = rebuild_heap_words(REPLACE_KEYS * 8 + 1024);
                for key in (shard..REPLACE_KEYS).step_by(REPLACE_THREADS) {
                    let count = shared
                        .with_write(|view| view.select_replace(&specs[key], atoms, &mut out))
                        .expect("no poisoned lock")
                        .expect("select_replace on a set succeeds");
                    assert_eq!(count, 1, "key {key} must be replaced exactly once");
                    replaced.fetch_add(count, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(
        replaced.load(Ordering::Relaxed),
        REPLACE_KEYS,
        "every key's rewrite is counted exactly once"
    );
    let expected: Vec<(i64, i64)> = (0..REPLACE_KEYS as i64)
        .map(|key| (key, key * 1000 + 1))
        .collect();
    assert_eq!(
        rendering(&shared),
        expected,
        "every object carries the value its own spec constructed"
    );
}

// ---------------------------------------------------------------------------
// 4. Live-oracle cross-check of the contended insert_new workload
// ---------------------------------------------------------------------------

/// Processes the oracle cross-check spawns.
const ORACLE_WORKERS: usize = 16;
/// Wall-clock bound for the oracle child, so a wedged oracle fails instead of hanging.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The embedded oracle program (an `escript`): `N` processes each perform one
/// `ets:insert_new/2` of the *same* key with a *different* value, and it prints how
/// many of them won (`winners -> 1` in the real `ets`), the resulting table size and
/// the stored value.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main([NStr]) ->
    N = list_to_integer(NStr),
    Tab = ets:new(contended, [set, public]),
    Parent = self(),
    Workers = [spawn_link(fun() ->
                   Result = ets:insert_new(Tab, {1, I}),
                   Parent ! {done, Result}
               end) || I <- lists:seq(1, N)],
    Results = [receive {done, Result} -> Result end || _ <- Workers],
    Winners = length([ok || true <- Results]),
    io:format("winners -> ~p~n", [Winners]),
    io:format("size -> ~p~n", [ets:info(Tab, size)]),
    io:format("value -> ~p~n", [ets:lookup_element(Tab, 1, 2)]),
    ets:delete(Tab),
    ok.
"#;

/// The integer on the oracle's `<tag> -> <n>` line.
fn parse_tag(stdout: &[u8], tag: &str) -> Option<i64> {
    let prefix = format!("{tag} -> ");
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix)?.trim().parse().ok())
}

/// The contended `insert_new` workload must produce the oracle's exact outcome:
/// exactly one winner, one stored object, one of the offered values.
#[test]
fn the_insert_new_winner_count_agrees_with_the_live_oracle() {
    const NAME: &str = "the_insert_new_winner_count_agrees_with_the_live_oracle";
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            NAME,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };

    let args = [ORACLE_WORKERS.to_string()];
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &args, "contended", ORACLE_TIMEOUT);
    let oracle_winners = parse_tag(&output.stdout, "winners").unwrap_or_else(|| {
        panic!(
            "the oracle printed no `winners ->` line:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    let oracle_size = parse_tag(&output.stdout, "size").expect("the oracle printed a `size ->` line");
    let oracle_value =
        parse_tag(&output.stdout, "value").expect("the oracle printed a `value ->` line");

    // The same workload through `beam_ets`.
    let atoms = AtomTable::new();
    let shared = shared_table(TableKind::Set);
    let winners = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for thread in 0..ORACLE_WORKERS {
            let shared = shared.clone();
            let atoms = &atoms;
            let winners = &winners;
            scope.spawn(move || {
                let mut heap = Heap::new(8);
                let candidate = object(&mut heap, CONTENDED_KEY, thread as i64 + 1);
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
    let rust_winners = winners.load(Ordering::Relaxed) as i64;
    let rust_size = shared.with_read(|view| view.size()).unwrap() as i64;

    println!(
        "[concurrency] contended insert_new: oracle winners={oracle_winners} size={oracle_size} \
         value={oracle_value} | rust winners={rust_winners} size={rust_size} \
         ({ORACLE_WORKERS} workers)"
    );
    assert_eq!(
        oracle_winners, 1,
        "the oracle's own winner count is exactly one"
    );
    assert_eq!(
        rust_winners, oracle_winners,
        "beam_ets and the live oracle must agree on the winner count"
    );
    assert_eq!(
        rust_size, oracle_size,
        "beam_ets and the live oracle must agree on the resulting table size"
    );
    assert!(
        (1..=ORACLE_WORKERS as i64).contains(&oracle_value),
        "the oracle's stored value is one of the offered ones, found {oracle_value}"
    );
}
