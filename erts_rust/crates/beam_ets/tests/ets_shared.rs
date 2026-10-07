//! Thread-safety of a shared ETS table: `Table: Send + Sync`, the
//! [`SharedTable`] lock wrapper, and the "no lost updates" guarantee under
//! concurrent writers.
//!
//! The oracle for every *result* here is ordinary single-threaded `ets`: the
//! point of the suite is that concurrency changes nothing observable. Each thread
//! inserts a **disjoint** set of keys, so the finished table must hold exactly
//! `THREADS × PER_THREAD` objects whatever the interleaving — a lost update would
//! show up as a smaller size.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use beam_ets::{Access, SharedTable, Table, TableId, TableOptions};
use beam_term::prelude::*;

use common::object;

/// Workers in the stress tests.
const THREADS: usize = 8;
/// Inserts performed by each worker.
const PER_THREAD: usize = 500;

/// Compile-time proof that both handles may cross thread boundaries and be shared
/// behind a reference (`std::thread::scope` requires exactly this).
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn table_and_shared_table_are_send_and_sync() {
    assert_send_sync::<Table>();
    assert_send_sync::<SharedTable>();
}

/// A still-open table (the tests drive several requesters explicitly).
fn open_table(kind: beam_ets::TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_access(Access::Public);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A `beam_term::heap::Heap` with room for `PER_THREAD` three-word tuples.
fn worker_heap() -> Heap {
    Heap::new(PER_THREAD * 4 + 16)
}

#[test]
fn concurrent_writers_lose_no_updates() {
    let shared = SharedTable::new(open_table(beam_ets::TableKind::Set));
    // The test terms are small-integer tuples, so `eq`/`term_hash` never consult an
    // atom table; each worker can therefore own an empty one without any risk of
    // disagreeing about an atom's index. (A shared `AtomTable` would need its own
    // lock, which is the runtime's concern — see the module docs of
    // `src/table/shared.rs`.)
    let atoms = AtomTable::new();

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let mut heap = worker_heap();
                for i in 0..PER_THREAD {
                    let key = (thread * PER_THREAD + i) as i64;
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
        THREADS * PER_THREAD,
        "every writer's disjoint keys must all be present — a lost update \
         would leave the table short"
    );
    // The same total is visible through an independent enumeration.
    assert_eq!(shared.with_read(|table| table.tab2list().len()).unwrap(), THREADS * PER_THREAD);
}

/// Readers running against a writer never observe a broken table: each read sees
/// a self-consistent size (`tab2list().len() == size()`), and the sizes only ever
/// grow until all writers are done.
#[test]
fn readers_never_observe_an_inconsistent_table() {
    let shared = SharedTable::new(open_table(beam_ets::TableKind::Set));
    let atoms = AtomTable::new();
    let reads = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let shared = shared.clone();
            let atoms = &atoms;
            scope.spawn(move || {
                let mut heap = worker_heap();
                for i in 0..PER_THREAD {
                    let key = (thread * PER_THREAD + i) as i64;
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
                for _ in 0..PER_THREAD {
                    // The read guard makes the two observations atomic with respect
                    // to every writer, so they can never disagree.
                    let (size, listed) = shared
                        .with_read(|table| (table.size(), table.tab2list().len()))
                        .unwrap();
                    assert_eq!(size, listed, "a locked read is internally consistent");
                    assert!(size <= THREADS * PER_THREAD);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(reads.load(Ordering::Relaxed), 4 * PER_THREAD);
    assert_eq!(shared.with_read(|table| table.size()).unwrap(), THREADS * PER_THREAD);
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
    let mut table = open_table(beam_ets::TableKind::OrderedSet);
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
